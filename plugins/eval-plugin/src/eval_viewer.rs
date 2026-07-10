//! Plugin-owned eval picker and run viewer surfaces.

use crate::eval_data::{
    EvalCampaignData, EvalCampaignSummary, EvalRunData, EvalRunSummary, best_variant,
    campaign_case_history, campaign_metric_names, case_avg_metric, diff_variant_count,
    discover_campaigns, discover_runs, discover_suites, format_duration_ms, format_number,
    load_repetition_artifact, run_avg_measurement, run_best_score, run_pass_rate,
    sum_variant_metric,
};
use bcode_eval_models::{
    EvalImprovementGeneration, EvalImprovementObjective, EvalRepetitionResult,
};
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface};
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowOutcome, ActionRowState};
use bmux_tui_components::bar_chart::{
    BarChart, BarChartItem, BarChartPolicy, BarChartStyles, BarChartValuePlacement,
};
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::dialog::{Dialog, DialogOutcome, DialogState};
use bmux_tui_components::modal_frame::{ModalSizing, ModalTheme};
use bmux_tui_components::sparkline::{Sparkline, SparklinePolicy, SparklineStyles};
use bmux_tui_components::tab_bar::{TabBar, TabBarOutcome, TabBarState, TabBarStyles, TabItem};
use bmux_tui_components::table::{
    Table, TableAlign, TableColumn, TableOutcome, TableRow, TableState, TableStyles,
};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBox, TextInputBoxOutcome, TextInputBoxPolicy};
use std::path::PathBuf;
use std::sync::mpsc::{self as std_mpsc, Receiver};

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
    active_wizard: Option<EvalWizard>,
    run_task: Option<Receiver<Result<bcode_eval_models::EvalRunResult, String>>>,
    status: String,
    table_area: Rect,
    action_area: Rect,
    surface_area: Rect,
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
            active_wizard: None,
            run_task: None,
            status,
            table_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
            surface_area: Rect::new(0, 0, 0, 0),
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

    fn selected_overview_row(&self) -> Option<OverviewRow> {
        let index = self.table_state.selected()?;
        self.overview_rows().get(index).copied()
    }

    fn selected_run_summary(&self) -> Option<&EvalRunSummary> {
        match self.selected_overview_row()? {
            OverviewRow::Run(index) => self.runs.get(index),
            OverviewRow::Campaign(_) => None,
        }
    }

    fn start_run_task(&mut self, options: bcode_eval::EvalRunOptions, host: &dyn PluginTuiHost) {
        if self.run_task.is_some() {
            self.status = "an eval suite is already running".to_string();
            return;
        }
        let (sender, receiver) = std_mpsc::channel();
        host.spawn_blocking(Box::new(move || {
            let result = bcode_eval::run_suite(&options).map_err(|error| error.to_string());
            let _ = sender.send(result);
        }));
        self.run_task = Some(receiver);
        self.status = "running eval suite in background...".to_string();
    }

    fn poll_run_task(&mut self) -> PluginTuiAction {
        let Some(receiver) = self.run_task.as_ref() else {
            return PluginTuiAction::None;
        };
        match receiver.try_recv() {
            Ok(Ok(run)) => {
                self.run_task = None;
                self.status = format!("completed run {}", run.manifest.run_id);
                self.refresh();
                PluginTuiAction::Redraw
            }
            Ok(Err(error)) => {
                self.run_task = None;
                self.status = format!("failed to run suite: {error}");
                PluginTuiAction::Redraw
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.run_task = None;
                self.status = "eval suite task disconnected".to_string();
                PluginTuiAction::Redraw
            }
            Err(std_mpsc::TryRecvError::Empty) => PluginTuiAction::None,
        }
    }

    fn run_suite(&mut self) {
        match EvalWizard::run_suite_from_runs(&self.runs, &self.runs_root) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn new_campaign(&mut self) {
        match EvalWizard::new_campaign_from_runs(&self.runs, &self.campaigns_root) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn show_help(&mut self) {
        self.active_wizard = Some(EvalWizard::help(
            "Eval Home Help",
            vec![
                "Open: open selected run or campaign",
                "Run Suite: execute a discovered suite",
                "New Campaign: create a campaign from discovered runs",
                "Start Campaign: create from selected run",
                "Attach Run: record selected run into a campaign",
                "Refresh: reload runs and campaigns",
                "Every action is clickable; keyboard shortcuts call the same actions.",
            ],
        ));
    }

    fn start_campaign_from_selected_run(&mut self) {
        let Some(run) = self.selected_run_summary() else {
            self.status = "select a run to start a campaign".to_string();
            return;
        };
        match EvalWizard::start_campaign_from_run(run, &self.campaigns_root) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn attach_selected_run_to_campaign(&mut self) {
        let Some(run) = self.selected_run_summary() else {
            self.status = "select a run to attach to a campaign".to_string();
            return;
        };
        match EvalWizard::attach_run_to_campaign(run, &self.campaigns) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn complete_wizard(&mut self, completion: EvalWizardCompletion, host: &dyn PluginTuiHost) {
        match completion {
            EvalWizardCompletion::StartCampaign(completion) => {
                match bcode_eval::start_improvement_campaign(
                    completion.suite_path,
                    completion.options,
                ) {
                    Ok(campaign) => {
                        self.status = format!("created campaign {}", campaign.id);
                        self.refresh();
                    }
                    Err(error) => self.status = format!("failed to create campaign: {error}"),
                }
            }
            EvalWizardCompletion::RunSuite(options) => self.start_run_task(*options, host),
            EvalWizardCompletion::RecordGeneration(options) => {
                match bcode_eval::record_improvement_generation(*options) {
                    Ok(generation) => {
                        self.status = format!("recorded generation {}", generation.id);
                        self.refresh();
                    }
                    Err(error) => self.status = format!("failed to record generation: {error}"),
                }
            }
            EvalWizardCompletion::DecideGeneration(_) => {
                self.status = "generation decisions are only available in campaigns".to_string();
            }
        }
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
            "start-campaign" => {
                self.start_campaign_from_selected_run();
                PluginTuiAction::Redraw
            }
            "run-suite" => {
                self.run_suite();
                PluginTuiAction::Redraw
            }
            "new-campaign" => {
                self.new_campaign();
                PluginTuiAction::Redraw
            }
            "attach-run" => {
                self.attach_selected_run_to_campaign();
                PluginTuiAction::Redraw
            }
            "refresh" => {
                self.refresh();
                PluginTuiAction::Redraw
            }
            "help" => {
                self.show_help();
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
        self.surface_area = area;
        if let Some(viewer) = self.embedded_viewer.as_mut() {
            viewer.render(area, frame);
            return;
        }
        if let Some(viewer) = self.embedded_campaign.as_mut() {
            viewer.render(area, frame);
            return;
        }
        if let Some(wizard) = self.active_wizard.as_mut() {
            wizard.render(area, frame);
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
        if let Some(wizard) = self.active_wizard.as_mut() {
            match wizard.handle_event(self.surface_area, event) {
                EvalWizardOutcome::Continue => return PluginTuiAction::None,
                EvalWizardOutcome::Redraw => return PluginTuiAction::Redraw,
                EvalWizardOutcome::Cancel => {
                    self.active_wizard = None;
                    return PluginTuiAction::Redraw;
                }
                EvalWizardOutcome::Complete(completion) => {
                    self.active_wizard = None;
                    self.complete_wizard(completion, host);
                    return PluginTuiAction::Redraw;
                }
            }
        }
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
                KeyCode::Char('s') => {
                    self.start_campaign_from_selected_run();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('u') => {
                    self.run_suite();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('n') => {
                    self.new_campaign();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('a') => {
                    self.attach_selected_run_to_campaign();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('r') => {
                    self.refresh();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('?') => {
                    self.show_help();
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

    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        let task_action = self.poll_run_task();
        if !matches!(task_action, PluginTuiAction::None) {
            return task_action;
        }
        if let Some(viewer) = self.embedded_campaign.as_mut() {
            return viewer.poll(host);
        }
        PluginTuiAction::None
    }
}

#[derive(Debug, Clone)]
enum EvalWizard {
    StartCampaign(Box<StartCampaignWizard>),
    RunSuite(Box<RunSuiteWizard>),
    RecordGeneration(Box<RecordGenerationWizard>),
    DecideGeneration(Box<DecideGenerationWizard>),
    Help(HelpWizard),
}

#[derive(Debug, Clone)]
struct HelpWizard {
    state: DialogState,
    title: &'static str,
    body: Vec<Line>,
}

#[derive(Debug, Clone)]
struct RunSuiteWizard {
    state: DialogState,
    suite_choices: Vec<StartCampaignSuiteChoice>,
    suite_index: usize,
    output_root: PathBuf,
    run_id: TextInputState,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct DecideGenerationWizard {
    state: DialogState,
    campaign: PathBuf,
    generation_id: String,
    status: bcode_eval_models::EvalImprovementVerdictStatus,
    context: Vec<Line>,
    rationale: TextInputState,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct StartCampaignWizard {
    state: DialogState,
    suite_choices: Vec<StartCampaignSuiteChoice>,
    suite_index: usize,
    output_root: PathBuf,
    campaign_id: TextInputState,
    name: TextInputState,
    focus: StartCampaignField,
    objective: EvalImprovementObjective,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct StartCampaignSuiteChoice {
    suite_id: String,
    suite_path: PathBuf,
    baseline_run: Option<PathBuf>,
    run_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartCampaignField {
    CampaignId,
    Name,
    Suite,
    Objective,
}

#[derive(Debug, Clone)]
struct RecordGenerationWizard {
    state: DialogState,
    campaign_choices: Vec<RecordCampaignChoice>,
    campaign_index: usize,
    parent_choices: Vec<RecordParentChoice>,
    parent_index: usize,
    run_choices: Vec<RecordRunChoice>,
    run_index: usize,
    allow_duplicate_run: bool,
    branch: TextInputState,
    delta_kind: bcode_eval_models::EvalImprovementDeltaKind,
    risk: bcode_eval_models::EvalImprovementRisk,
    context: Vec<Line>,
    summary: TextInputState,
    rationale: TextInputState,
    patch_path: TextInputState,
    overlays: TextInputState,
    affected_files: TextInputState,
    affected_surfaces: TextInputState,
    expected_impact: TextInputState,
    focus: RecordGenerationField,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct RecordCampaignChoice {
    label: String,
    campaign_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct RecordParentChoice {
    label: String,
    parent_id: Option<String>,
}

#[derive(Debug, Clone)]
struct RecordRunChoice {
    label: String,
    run_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordGenerationField {
    Summary,
    Campaign,
    Parent,
    Run,
    Branch,
    Patch,
    Overlays,
    AffectedFiles,
    AffectedSurfaces,
    ExpectedImpact,
    Kind,
    Risk,
    Rationale,
}

#[derive(Debug, Clone)]
enum EvalWizardOutcome {
    Continue,
    Redraw,
    Cancel,
    Complete(EvalWizardCompletion),
}

#[derive(Debug, Clone)]
struct StartCampaignCompletion {
    suite_path: PathBuf,
    options: bcode_eval::EvalImprovementStartOptions,
}

#[derive(Debug, Clone)]
enum EvalWizardCompletion {
    StartCampaign(Box<StartCampaignCompletion>),
    RunSuite(Box<bcode_eval::EvalRunOptions>),
    RecordGeneration(Box<bcode_eval::EvalImprovementRecordOptions>),
    DecideGeneration(Box<bcode_eval::EvalImprovementDecisionOptions>),
}

impl EvalWizard {
    fn help(title: &'static str, lines: Vec<&'static str>) -> Self {
        Self::Help(HelpWizard {
            state: DialogState::new(),
            title,
            body: lines.into_iter().map(Line::from).collect(),
        })
    }

    fn run_suite_from_runs(
        runs: &[EvalRunSummary],
        output_root: &std::path::Path,
    ) -> Result<Self, String> {
        let choices = suite_choices_from_runs(runs);
        if choices.is_empty() {
            return Err("no suites with recorded paths are available".to_string());
        }
        Ok(Self::RunSuite(Box::new(RunSuiteWizard {
            state: DialogState::new(),
            suite_choices: choices,
            suite_index: 0,
            output_root: output_root.to_path_buf(),
            run_id: text_state(""),
            error: None,
        })))
    }

    fn decide_generation(
        data: &EvalCampaignData,
        generation: &EvalImprovementGeneration,
        status: bcode_eval_models::EvalImprovementVerdictStatus,
    ) -> Result<Self, String> {
        if generation.id == data.campaign.baseline_generation_id {
            return Err("the baseline generation cannot be promoted or rejected".to_string());
        }
        Ok(Self::DecideGeneration(Box::new(DecideGenerationWizard {
            state: DialogState::new(),
            campaign: data.campaign_dir.clone(),
            generation_id: generation.id.clone(),
            status,
            context: vec![
                Line::from(format!("Risk: {:?}", generation.delta.risk)),
                Line::from(format!(
                    "Affected files: {}",
                    generation.delta.affected_files.len()
                )),
                Line::from(format!(
                    "Affected surfaces: {}",
                    generation.delta.affected_surfaces.join(", ")
                )),
                Line::from(format!(
                    "Metric deltas: {}",
                    generation
                        .vs_parent
                        .as_ref()
                        .map_or(0, std::collections::BTreeMap::len)
                )),
            ],
            rationale: text_state(""),
            error: None,
        })))
    }

    fn start_campaign_from_run(
        run: &EvalRunSummary,
        campaigns_root: &std::path::Path,
    ) -> Result<Self, String> {
        let run_data = EvalRunData::load(&run.run_dir)
            .map_err(|error| format!("failed to load selected run: {error}"))?;
        let suite_path = run_data
            .result
            .manifest
            .suite_path
            .ok_or_else(|| "selected run does not record a suite path".to_string())?;
        let choice = StartCampaignSuiteChoice {
            suite_id: run.suite_id.clone(),
            suite_path,
            baseline_run: Some(run.run_dir.clone()),
            run_id: Some(run.run_id.clone()),
        };
        Ok(Self::start_campaign_wizard(vec![choice], 0, campaigns_root))
    }

    fn new_campaign_from_runs(
        runs: &[EvalRunSummary],
        campaigns_root: &std::path::Path,
    ) -> Result<Self, String> {
        let choices = suite_choices_from_runs(runs);
        if choices.is_empty() {
            return Err("no valid eval suites were discovered".to_string());
        }
        Ok(Self::start_campaign_wizard(choices, 0, campaigns_root))
    }

    fn start_campaign_wizard(
        choices: Vec<StartCampaignSuiteChoice>,
        suite_index: usize,
        campaigns_root: &std::path::Path,
    ) -> Self {
        let choice = &choices[suite_index];
        let campaign_id =
            unique_campaign_id(campaigns_root, &format!("{}-improvement", choice.suite_id));
        let name = format!("{} improvement", choice.suite_id);
        Self::StartCampaign(Box::new(StartCampaignWizard {
            state: DialogState::new(),
            suite_choices: choices,
            suite_index,
            output_root: campaigns_root.to_path_buf(),
            campaign_id: text_state(&campaign_id),
            name: text_state(&name),
            focus: StartCampaignField::CampaignId,
            objective: EvalImprovementObjective::Progression,
            error: None,
        }))
    }

    fn attach_run_to_campaign(
        run: &EvalRunSummary,
        campaigns: &[EvalCampaignSummary],
    ) -> Result<Self, String> {
        let mut matching = campaigns
            .iter()
            .filter(|campaign| campaign.suite_id == run.suite_id)
            .collect::<Vec<_>>();
        matching.sort_by_key(|campaign| std::cmp::Reverse(campaign.modified_ms));
        let first = matching
            .first()
            .ok_or_else(|| format!("no campaign found for suite {}", run.suite_id))?;
        let campaign_choices = matching
            .iter()
            .map(|campaign| RecordCampaignChoice {
                label: campaign.campaign_id.clone(),
                campaign_dir: campaign.campaign_dir.clone(),
            })
            .collect::<Vec<_>>();
        let parent_id = first.latest_generation_id.clone();
        let summary = format!("Recorded run {}", run.run_id);
        let context = vec![
            Line::from("Attach this run as the next campaign generation."),
            Line::from(format!("Run: {}", run.run_id)),
        ];
        Ok(Self::RecordGeneration(Box::new(RecordGenerationWizard {
            state: DialogState::new(),
            campaign_choices,
            campaign_index: 0,
            parent_choices: vec![RecordParentChoice {
                label: parent_id.clone().unwrap_or_else(|| "latest".to_string()),
                parent_id,
            }],
            parent_index: 0,
            run_choices: vec![RecordRunChoice {
                label: run.run_id.clone(),
                run_dir: Some(run.run_dir.clone()),
            }],
            run_index: 0,
            allow_duplicate_run: false,
            branch: text_state("main"),
            delta_kind: bcode_eval_models::EvalImprovementDeltaKind::Mixed,
            risk: bcode_eval_models::EvalImprovementRisk::Low,
            context,
            summary: text_state(&summary),
            rationale: text_state("Recorded from the eval overview TUI."),
            patch_path: text_state(""),
            overlays: text_state(""),
            affected_files: text_state(""),
            affected_surfaces: text_state(""),
            expected_impact: text_state(""),
            focus: RecordGenerationField::Summary,
            error: None,
        })))
    }

    fn record_generation_for_campaign(
        data: &EvalCampaignData,
        selected: Option<&EvalImprovementGeneration>,
    ) -> Result<Self, String> {
        let (run_choices, run_index) = campaign_run_choices(data)?;
        let run = &run_choices[run_index];
        let parent_id = selected
            .map(|generation| generation.id.clone())
            .or_else(|| data.campaign.latest_generation_id.clone());
        let parent_choices = campaign_parent_choices(data);
        let parent_index = parent_id
            .as_ref()
            .and_then(|id| {
                parent_choices
                    .iter()
                    .position(|choice| choice.parent_id.as_ref() == Some(id))
            })
            .unwrap_or(0);
        let summary = format!("Recorded {}", run.label);
        let context = vec![
            Line::from("Record a run or metadata-only generation."),
            Line::from(format!("Run: {}", run.label)),
        ];
        Ok(Self::RecordGeneration(Box::new(RecordGenerationWizard {
            state: DialogState::new(),
            campaign_choices: vec![RecordCampaignChoice {
                label: data.campaign.id.clone(),
                campaign_dir: data.campaign_dir.clone(),
            }],
            campaign_index: 0,
            parent_choices,
            parent_index,
            run_choices,
            run_index,
            allow_duplicate_run: false,
            branch: text_state("main"),
            delta_kind: bcode_eval_models::EvalImprovementDeltaKind::Mixed,
            risk: bcode_eval_models::EvalImprovementRisk::Low,
            context,
            summary: text_state(&summary),
            rationale: text_state("Recorded from the campaign timeline TUI."),
            patch_path: text_state(""),
            overlays: text_state(""),
            affected_files: text_state(""),
            affected_surfaces: text_state(""),
            expected_impact: text_state(""),
            focus: RecordGenerationField::Summary,
            error: None,
        })))
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        match self {
            Self::StartCampaign(wizard) => wizard.render(area, frame),
            Self::RunSuite(wizard) => wizard.render(area, frame),
            Self::RecordGeneration(wizard) => wizard.render(area, frame),
            Self::DecideGeneration(wizard) => wizard.render(area, frame),
            Self::Help(wizard) => wizard.render(area, frame),
        }
    }

    fn handle_event(&mut self, area: Rect, event: &Event) -> EvalWizardOutcome {
        if let Event::Key(stroke) = event {
            match stroke.key {
                KeyCode::Escape => return EvalWizardOutcome::Cancel,
                KeyCode::Tab => {
                    self.focus_next();
                    return EvalWizardOutcome::Redraw;
                }
                _ => {}
            }
        }
        if self.handle_inputs(area, event) {
            return EvalWizardOutcome::Redraw;
        }
        if let Event::Key(stroke) = event {
            match stroke.key {
                KeyCode::Enter => return self.complete(),
                KeyCode::Left => {
                    self.cycle_choice(false);
                    return EvalWizardOutcome::Redraw;
                }
                KeyCode::Right => {
                    self.cycle_choice(true);
                    return EvalWizardOutcome::Redraw;
                }
                _ => {}
            }
        }
        let actions = match self {
            Self::StartCampaign(_) => wizard_actions("create"),
            Self::RunSuite(_) => wizard_actions("run"),
            Self::RecordGeneration(_) => wizard_actions("record"),
            Self::DecideGeneration(wizard) => wizard_actions(match wizard.status {
                bcode_eval_models::EvalImprovementVerdictStatus::Promoted => "promote",
                _ => "reject",
            }),
            Self::Help(_) => vec![ActionButton::new("cancel", "Close")],
        };
        let outcome = Dialog::new(&[], &actions, eval_modal_theme())
            .title(self.title())
            .sizing(wizard_sizing())
            .handle_event(area, self.dialog_state_mut(), event);
        match outcome {
            DialogOutcome::Ignored => EvalWizardOutcome::Continue,
            DialogOutcome::Redraw => EvalWizardOutcome::Redraw,
            DialogOutcome::Action { id, .. } if id == "cancel" => EvalWizardOutcome::Cancel,
            DialogOutcome::Action { .. } => self.complete(),
        }
    }

    const fn title(&self) -> &'static str {
        match self {
            Self::StartCampaign(_) => "Start Improvement Campaign",
            Self::RunSuite(_) => "Run Eval Suite",
            Self::RecordGeneration(_) => "Record Generation",
            Self::DecideGeneration(wizard) => match wizard.status {
                bcode_eval_models::EvalImprovementVerdictStatus::Promoted => "Promote Generation",
                _ => "Reject Generation",
            },
            Self::Help(wizard) => wizard.title,
        }
    }

    const fn dialog_state_mut(&mut self) -> &mut DialogState {
        match self {
            Self::StartCampaign(wizard) => &mut wizard.state,
            Self::RunSuite(wizard) => &mut wizard.state,
            Self::RecordGeneration(wizard) => &mut wizard.state,
            Self::DecideGeneration(wizard) => &mut wizard.state,
            Self::Help(wizard) => &mut wizard.state,
        }
    }

    const fn focus_next(&mut self) {
        match self {
            Self::StartCampaign(wizard) => {
                wizard.focus = match wizard.focus {
                    StartCampaignField::CampaignId => StartCampaignField::Name,
                    StartCampaignField::Name => StartCampaignField::Suite,
                    StartCampaignField::Suite => StartCampaignField::Objective,
                    StartCampaignField::Objective => StartCampaignField::CampaignId,
                }
            }
            Self::RecordGeneration(wizard) => {
                wizard.focus = match wizard.focus {
                    RecordGenerationField::Summary => RecordGenerationField::Campaign,
                    RecordGenerationField::Campaign => RecordGenerationField::Parent,
                    RecordGenerationField::Parent => RecordGenerationField::Run,
                    RecordGenerationField::Run => RecordGenerationField::Branch,
                    RecordGenerationField::Branch => RecordGenerationField::Patch,
                    RecordGenerationField::Patch => RecordGenerationField::Overlays,
                    RecordGenerationField::Overlays => RecordGenerationField::AffectedFiles,
                    RecordGenerationField::AffectedFiles => RecordGenerationField::AffectedSurfaces,
                    RecordGenerationField::AffectedSurfaces => {
                        RecordGenerationField::ExpectedImpact
                    }
                    RecordGenerationField::ExpectedImpact => RecordGenerationField::Kind,
                    RecordGenerationField::Kind => RecordGenerationField::Risk,
                    RecordGenerationField::Risk => RecordGenerationField::Rationale,
                    RecordGenerationField::Rationale => RecordGenerationField::Summary,
                }
            }
            Self::RunSuite(_) | Self::DecideGeneration(_) | Self::Help(_) => {}
        }
    }

    fn cycle_choice(&mut self, forward: bool) {
        match self {
            Self::StartCampaign(wizard) if wizard.focus == StartCampaignField::Suite => {
                wizard.suite_index =
                    cycle_index(wizard.suite_choices.len(), wizard.suite_index, forward);
            }
            Self::RunSuite(wizard) => {
                wizard.suite_index =
                    cycle_index(wizard.suite_choices.len(), wizard.suite_index, forward);
            }
            Self::StartCampaign(wizard) if wizard.focus == StartCampaignField::Objective => {
                wizard.objective = cycle_objective(wizard.objective, forward);
            }
            Self::RecordGeneration(wizard) if wizard.focus == RecordGenerationField::Campaign => {
                wizard.campaign_index = cycle_index(
                    wizard.campaign_choices.len(),
                    wizard.campaign_index,
                    forward,
                );
            }
            Self::RecordGeneration(wizard) if wizard.focus == RecordGenerationField::Parent => {
                wizard.parent_index =
                    cycle_index(wizard.parent_choices.len(), wizard.parent_index, forward);
            }
            Self::RecordGeneration(wizard) if wizard.focus == RecordGenerationField::Run => {
                wizard.run_index = cycle_index(wizard.run_choices.len(), wizard.run_index, forward);
            }
            Self::RecordGeneration(wizard) if wizard.focus == RecordGenerationField::Kind => {
                wizard.delta_kind = cycle_delta_kind(wizard.delta_kind, forward);
            }
            Self::RecordGeneration(wizard) if wizard.focus == RecordGenerationField::Risk => {
                wizard.risk = cycle_risk(wizard.risk, forward);
            }
            _ => {}
        }
    }

    fn handle_inputs(&mut self, area: Rect, event: &Event) -> bool {
        match self {
            Self::StartCampaign(wizard) => wizard.handle_inputs(area, event),
            Self::RunSuite(wizard) => wizard.handle_inputs(area, event),
            Self::RecordGeneration(wizard) => wizard.handle_inputs(area, event),
            Self::DecideGeneration(wizard) => wizard.handle_inputs(area, event),
            Self::Help(_) => false,
        }
    }

    fn complete(&mut self) -> EvalWizardOutcome {
        match self {
            Self::StartCampaign(wizard) => match wizard.validate() {
                Ok(()) => EvalWizardOutcome::Complete(EvalWizardCompletion::StartCampaign(
                    Box::new(StartCampaignCompletion {
                        suite_path: wizard.suite_path(),
                        options: wizard.options(),
                    }),
                )),
                Err(error) => {
                    wizard.error = Some(error);
                    EvalWizardOutcome::Redraw
                }
            },
            Self::RunSuite(wizard) => match wizard.validate() {
                Ok(()) => EvalWizardOutcome::Complete(EvalWizardCompletion::RunSuite(Box::new(
                    wizard.options(),
                ))),
                Err(error) => {
                    wizard.error = Some(error);
                    EvalWizardOutcome::Redraw
                }
            },
            Self::RecordGeneration(wizard) => match wizard.validate() {
                Ok(()) => EvalWizardOutcome::Complete(EvalWizardCompletion::RecordGeneration(
                    Box::new(wizard.options()),
                )),
                Err(error) => {
                    wizard.error = Some(error);
                    EvalWizardOutcome::Redraw
                }
            },
            Self::DecideGeneration(wizard) => match wizard.validate() {
                Ok(()) => EvalWizardOutcome::Complete(EvalWizardCompletion::DecideGeneration(
                    Box::new(wizard.options()),
                )),
                Err(error) => {
                    wizard.error = Some(error);
                    EvalWizardOutcome::Redraw
                }
            },
            Self::Help(_) => EvalWizardOutcome::Cancel,
        }
    }
}

impl RunSuiteWizard {
    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        let choice = &self.suite_choices[self.suite_index];
        let mut body = vec![
            Line::from(format!("Suite: {}", choice.suite_id)),
            Line::from(format!("Path: {}", choice.suite_path.display())),
            Line::from("Use arrows or click Suite to cycle choices."),
        ];
        if let Some(error) = &self.error {
            body.push(Line::from(format!("Error: {error}")));
        }
        Dialog::new(&body, &wizard_actions("run"), eval_modal_theme())
            .title("Run Eval Suite")
            .sizing(wizard_sizing())
            .render(area, &self.state, frame);
        let layout = wizard_layout(area);
        render_input_box(
            layout.primary,
            frame,
            "Run id (optional)",
            &mut self.run_id,
            true,
            1,
        );
    }

    fn handle_inputs(&mut self, area: Rect, event: &Event) -> bool {
        let layout = wizard_layout(area);
        if event_click_in(event, layout.choice) {
            self.suite_index = cycle_index(self.suite_choices.len(), self.suite_index, true);
            return true;
        }
        handle_input_box(layout.primary, &mut self.run_id, event, true)
    }

    fn validate(&self) -> Result<(), String> {
        let choice = &self.suite_choices[self.suite_index];
        bcode_eval::load_suite(&choice.suite_path)
            .map(|_| ())
            .map_err(|error| format!("suite is invalid: {error}"))
    }

    fn options(&self) -> bcode_eval::EvalRunOptions {
        let choice = &self.suite_choices[self.suite_index];
        let run_id = input_text(&self.run_id);
        bcode_eval::EvalRunOptions {
            suite_path: choice.suite_path.clone(),
            output_root: self.output_root.clone(),
            run_id: (!run_id.is_empty()).then_some(run_id),
        }
    }
}

impl DecideGenerationWizard {
    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        let action = match self.status {
            bcode_eval_models::EvalImprovementVerdictStatus::Promoted => "promote",
            _ => "reject",
        };
        let mut body = vec![
            Line::from(format!("Generation: {}", self.generation_id)),
            Line::from(format!("Decision: {action}")),
            Line::from("A rationale is required."),
        ];
        body.extend(self.context.clone());
        if let Some(error) = &self.error {
            body.push(Line::from(format!("Error: {error}")));
        }
        Dialog::new(&body, &wizard_actions(action), eval_modal_theme())
            .title(if action == "promote" {
                "Promote Generation"
            } else {
                "Reject Generation"
            })
            .sizing(wizard_sizing())
            .render(area, &self.state, frame);
        render_input_box(
            wizard_layout(area).primary,
            frame,
            "Rationale",
            &mut self.rationale,
            true,
            1,
        );
    }

    fn handle_inputs(&mut self, area: Rect, event: &Event) -> bool {
        handle_input_box(
            wizard_layout(area).primary,
            &mut self.rationale,
            event,
            true,
        )
    }

    fn validate(&self) -> Result<(), String> {
        if input_text(&self.rationale).is_empty() {
            Err("decision rationale is required".to_string())
        } else {
            Ok(())
        }
    }

    fn options(&self) -> bcode_eval::EvalImprovementDecisionOptions {
        bcode_eval::EvalImprovementDecisionOptions {
            campaign: self.campaign.clone(),
            generation_id: self.generation_id.clone(),
            status: self.status,
            rationale: input_text(&self.rationale),
            actor: std::env::var("USER")
                .ok()
                .filter(|actor| !actor.trim().is_empty()),
        }
    }
}

impl HelpWizard {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        Dialog::new(
            &self.body,
            &[ActionButton::new("cancel", "Close")],
            eval_modal_theme(),
        )
        .title(self.title)
        .sizing(wizard_sizing())
        .render(area, &self.state, frame);
    }
}

impl StartCampaignWizard {
    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        let actions = wizard_actions("create");
        let choice = self.selected_choice();
        let run_label = choice.run_id.as_deref().unwrap_or("no baseline run");
        let mut body = vec![
            Line::from("Create an improvement campaign."),
            Line::from(format!("Run: {run_label}")),
            Line::from(format!("Suite: {}", choice.suite_id)),
            Line::from(format!("Suite selector: {}", choice.suite_id)),
            Line::from(""),
            Line::from(""),
            Line::from(""),
            Line::from(format!("Objective: {}", objective_label(self.objective))),
            Line::from("Click fields/buttons or use Tab and Esc. Enter submits the field."),
        ];
        if let Some(error) = &self.error {
            body.push(Line::from(format!("Error: {error}")));
        }
        Dialog::new(&body, &actions, eval_modal_theme())
            .title("Start Improvement Campaign")
            .sizing(wizard_sizing())
            .render(area, &self.state, frame);
        let layout = wizard_layout(area);
        render_input_box(
            layout.primary,
            frame,
            "Campaign id",
            &mut self.campaign_id,
            self.focus == StartCampaignField::CampaignId,
            1,
        );
        render_input_box(
            layout.secondary,
            frame,
            "Campaign name",
            &mut self.name,
            self.focus == StartCampaignField::Name,
            1,
        );
    }

    fn handle_inputs(&mut self, area: Rect, event: &Event) -> bool {
        let layout = wizard_layout(area);
        if event_click_in(event, layout.primary) {
            self.focus = StartCampaignField::CampaignId;
        } else if event_click_in(event, layout.secondary) {
            self.focus = StartCampaignField::Name;
        } else if event_click_in(event, layout.choice) {
            self.focus = StartCampaignField::Suite;
            self.suite_index = cycle_index(self.suite_choices.len(), self.suite_index, true);
            return true;
        } else if event_click_in(event, layout.choice_alt) {
            self.focus = StartCampaignField::Objective;
            self.objective = cycle_objective(self.objective, true);
            return true;
        }
        let id = handle_input_box(
            layout.primary,
            &mut self.campaign_id,
            event,
            self.focus == StartCampaignField::CampaignId,
        );
        let name = handle_input_box(
            layout.secondary,
            &mut self.name,
            event,
            self.focus == StartCampaignField::Name,
        );
        id || name
    }

    fn selected_choice(&self) -> &StartCampaignSuiteChoice {
        &self.suite_choices[self.suite_index]
    }

    fn suite_path(&self) -> PathBuf {
        self.selected_choice().suite_path.clone()
    }

    fn validate(&self) -> Result<(), String> {
        let campaign_id = input_text(&self.campaign_id);
        if campaign_id.is_empty() {
            return Err("campaign id is required".to_string());
        }
        if sanitize_id(&campaign_id) != campaign_id {
            return Err(
                "campaign id may only contain ASCII letters, numbers, '-' and '_'".to_string(),
            );
        }
        if input_text(&self.name).is_empty() {
            return Err("campaign name is required".to_string());
        }
        if !self.suite_path().exists() {
            return Err(format!(
                "suite path does not exist: {}",
                self.suite_path().display()
            ));
        }
        let output_path = self.output_root.join(&campaign_id);
        if output_path.exists() {
            return Err(format!(
                "campaign already exists: {}",
                output_path.display()
            ));
        }
        Ok(())
    }

    fn options(&self) -> bcode_eval::EvalImprovementStartOptions {
        bcode_eval::EvalImprovementStartOptions {
            output_root: self.output_root.clone(),
            campaign_id: Some(input_text(&self.campaign_id)),
            name: Some(input_text(&self.name)),
            baseline_run: self.selected_choice().baseline_run.clone(),
            objective: self.objective,
        }
    }
}

impl RecordGenerationWizard {
    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        let actions = wizard_actions("record");
        let mut body = self.context.clone();
        body.push(Line::from(format!(
            "Campaign: {}",
            self.selected_campaign().label
        )));
        body.push(Line::from(format!(
            "Parent: {}",
            self.selected_parent().label
        )));
        body.push(Line::from(format!("Run: {}", self.selected_run().label)));
        body.push(Line::from(format!(
            "Allow duplicate run: {} (D toggles)",
            if self.allow_duplicate_run {
                "yes"
            } else {
                "no"
            }
        )));
        body.push(Line::from(format!(
            "Kind: {}",
            delta_kind_label(self.delta_kind)
        )));
        body.push(Line::from(format!("Risk: {}", risk_label(self.risk))));
        body.push(Line::from(format!(
            "Branch: {}",
            empty_label(&input_text(&self.branch))
        )));
        body.push(Line::from(format!(
            "Metadata field: {}",
            self.metadata_field_label()
        )));
        body.push(Line::from(
            "Click fields/buttons or use Tab and Esc. Enter submits the field.",
        ));
        if let Some(error) = &self.error {
            body.push(Line::from(format!("Error: {error}")));
        }
        Dialog::new(&body, &actions, eval_modal_theme())
            .title("Record Generation")
            .sizing(wizard_sizing())
            .render(area, &self.state, frame);
        let layout = wizard_layout(area);
        render_input_box(
            layout.primary,
            frame,
            "Summary",
            &mut self.summary,
            self.focus == RecordGenerationField::Summary,
            1,
        );
        render_input_box(
            layout.secondary,
            frame,
            "Rationale",
            &mut self.rationale,
            self.focus == RecordGenerationField::Rationale,
            1,
        );
        let metadata_focus = self.metadata_focus();
        let metadata_label = self.metadata_field_label();
        let metadata = self.metadata_state_mut();
        render_input_box(
            layout.tertiary,
            frame,
            metadata_label,
            metadata,
            metadata_focus,
            1,
        );
    }

    fn handle_inputs(&mut self, area: Rect, event: &Event) -> bool {
        let layout = wizard_layout(area);
        if event_click_in(event, layout.primary) {
            self.focus = RecordGenerationField::Summary;
        } else if event_click_in(event, layout.secondary) {
            self.focus = RecordGenerationField::Rationale;
        } else if event_click_in(event, layout.tertiary) {
            if !self.metadata_focus() {
                self.focus = RecordGenerationField::Branch;
            }
        } else if event_click_in(event, layout.choice) {
            self.focus = RecordGenerationField::Campaign;
            self.campaign_index =
                cycle_index(self.campaign_choices.len(), self.campaign_index, true);
            return true;
        } else if event_click_in(event, layout.choice_alt) {
            self.focus = RecordGenerationField::Parent;
            self.parent_index = cycle_index(self.parent_choices.len(), self.parent_index, true);
            return true;
        } else if event_click_in(event, layout.choice_second) {
            self.focus = RecordGenerationField::Kind;
            self.delta_kind = cycle_delta_kind(self.delta_kind, true);
            return true;
        } else if event_click_in(event, layout.choice_second_alt) {
            self.focus = RecordGenerationField::Risk;
            self.risk = cycle_risk(self.risk, true);
            return true;
        } else if event_click_in(event, layout.choice_third) {
            self.focus = RecordGenerationField::Run;
            self.run_index = cycle_index(self.run_choices.len(), self.run_index, true);
            return true;
        }
        let summary = handle_input_box(
            layout.primary,
            &mut self.summary,
            event,
            self.focus == RecordGenerationField::Summary,
        );
        let rationale = handle_input_box(
            layout.secondary,
            &mut self.rationale,
            event,
            self.focus == RecordGenerationField::Rationale,
        );
        let metadata_focus = self.metadata_focus();
        let metadata = handle_input_box(
            layout.tertiary,
            self.metadata_state_mut(),
            event,
            metadata_focus,
        );
        summary || rationale || metadata
    }

    const fn metadata_focus(&self) -> bool {
        matches!(
            self.focus,
            RecordGenerationField::Branch
                | RecordGenerationField::Patch
                | RecordGenerationField::Overlays
                | RecordGenerationField::AffectedFiles
                | RecordGenerationField::AffectedSurfaces
                | RecordGenerationField::ExpectedImpact
        )
    }

    const fn metadata_field_label(&self) -> &'static str {
        match self.focus {
            RecordGenerationField::Branch => "Branch",
            RecordGenerationField::Overlays => "Overlay paths (comma-separated)",
            RecordGenerationField::AffectedFiles => "Affected files (comma-separated)",
            RecordGenerationField::AffectedSurfaces => "Affected surfaces (comma-separated)",
            RecordGenerationField::ExpectedImpact => "Expected impact",
            _ => "Patch path (optional)",
        }
    }

    const fn metadata_state_mut(&mut self) -> &mut TextInputState {
        match self.focus {
            RecordGenerationField::Branch => &mut self.branch,
            RecordGenerationField::Overlays => &mut self.overlays,
            RecordGenerationField::AffectedFiles => &mut self.affected_files,
            RecordGenerationField::AffectedSurfaces => &mut self.affected_surfaces,
            RecordGenerationField::ExpectedImpact => &mut self.expected_impact,
            _ => &mut self.patch_path,
        }
    }

    fn selected_campaign(&self) -> &RecordCampaignChoice {
        &self.campaign_choices[self.campaign_index]
    }

    fn selected_parent(&self) -> &RecordParentChoice {
        &self.parent_choices[self.parent_index]
    }

    fn selected_run(&self) -> &RecordRunChoice {
        &self.run_choices[self.run_index]
    }

    fn validate(&self) -> Result<(), String> {
        if input_text(&self.summary).is_empty() {
            return Err("summary is required".to_string());
        }
        if self.selected_run().label.ends_with(" (duplicate)") && !self.allow_duplicate_run {
            return Err("selected run is already attached; press D to allow duplicate".to_string());
        }
        let branch = input_text(&self.branch);
        if branch.is_empty() || sanitize_id(&branch) != branch {
            return Err(
                "branch may only contain lowercase ASCII letters, numbers, '-' and '_'".to_string(),
            );
        }
        let patch = input_text(&self.patch_path);
        if !patch.is_empty() && !std::path::Path::new(&patch).is_file() {
            return Err(format!("patch path is not a file: {patch}"));
        }
        for overlay in comma_paths(&input_text(&self.overlays)) {
            if !overlay.is_file() {
                return Err(format!("overlay path is not a file: {}", overlay.display()));
            }
        }
        Ok(())
    }

    fn options(&self) -> bcode_eval::EvalImprovementRecordOptions {
        bcode_eval::EvalImprovementRecordOptions {
            campaign: self.selected_campaign().campaign_dir.clone(),
            parent_id: self.selected_parent().parent_id.clone(),
            branch: input_text(&self.branch),
            delta_kind: self.delta_kind,
            summary: input_text(&self.summary),
            run: self.selected_run().run_dir.clone(),
            patch: optional_path(&input_text(&self.patch_path)),
            overlays: comma_paths(&input_text(&self.overlays)),
            affected_files: comma_paths(&input_text(&self.affected_files)),
            affected_surfaces: comma_values(&input_text(&self.affected_surfaces)),
            expected_impact: optional_text(&input_text(&self.expected_impact)),
            risk: self.risk,
            rationale: Some(input_text(&self.rationale)).filter(|text| !text.trim().is_empty()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct WizardLayout {
    primary: Rect,
    secondary: Rect,
    tertiary: Rect,
    choice: Rect,
    choice_alt: Rect,
    choice_second: Rect,
    choice_second_alt: Rect,
    choice_third: Rect,
}

fn wizard_layout(area: Rect) -> WizardLayout {
    let body = Dialog::new(&[], &wizard_actions("confirm"), eval_modal_theme())
        .title("Wizard")
        .sizing(wizard_sizing())
        .layout(area)
        .body;
    WizardLayout {
        primary: Rect::new(body.x, body.y.saturating_add(4), body.width, 4),
        secondary: Rect::new(body.x, body.y.saturating_add(8), body.width, 4),
        tertiary: Rect::new(body.x, body.y.saturating_add(12), body.width, 4),
        choice: Rect::new(body.x, body.y.saturating_add(17), body.width / 2, 1),
        choice_alt: Rect::new(
            body.x.saturating_add(body.width / 2),
            body.y.saturating_add(17),
            body.width / 2,
            1,
        ),
        choice_second: Rect::new(body.x, body.y.saturating_add(18), body.width / 2, 1),
        choice_second_alt: Rect::new(
            body.x.saturating_add(body.width / 2),
            body.y.saturating_add(18),
            body.width / 2,
            1,
        ),
        choice_third: Rect::new(body.x, body.y.saturating_add(19), body.width, 1),
    }
}

const fn wizard_sizing() -> ModalSizing {
    ModalSizing::new(Size::new(56, 24), Size::new(96, 34), Insets::all(2))
}

fn render_input_box(
    area: Rect,
    frame: &mut Frame<'_>,
    label: &'static str,
    state: &mut TextInputState,
    focused: bool,
    rows: u16,
) {
    TextInputBox::new(TextInputPolicy::chat_composer())
        .label(label)
        .policy(TextInputBoxPolicy {
            field_chrome: true,
            panel_chrome: true,
            background: true,
            cursor: true,
            focused,
            disabled: false,
            min_rows: rows,
            max_rows: Some(rows),
        })
        .render(area, state, frame);
}

fn handle_input_box(area: Rect, state: &mut TextInputState, event: &Event, focused: bool) -> bool {
    if !focused && !event_click_in(event, area) {
        return false;
    }
    matches!(
        TextInputBox::new(TextInputPolicy::chat_composer())
            .label("")
            .policy(TextInputBoxPolicy::labeled_field())
            .handle_event(area, state, event),
        TextInputBoxOutcome::Edited | TextInputBoxOutcome::Redraw | TextInputBoxOutcome::Submitted
    )
}

const fn event_click_in(event: &Event, area: Rect) -> bool {
    matches!(
        event,
        Event::Mouse(mouse)
            if matches!(mouse.kind, MouseEventKind::Down(_))
                && mouse.position.x >= area.x
                && mouse.position.x < area.x.saturating_add(area.width)
                && mouse.position.y >= area.y
                && mouse.position.y < area.y.saturating_add(area.height)
    )
}

fn text_state(value: &str) -> TextInputState {
    TextInputState::new(TextEditBuffer::from_text(value.to_string()))
}

fn input_text(state: &TextInputState) -> String {
    state.buffer().text().trim().to_string()
}

fn cycle_objective(objective: EvalImprovementObjective, forward: bool) -> EvalImprovementObjective {
    let items = [
        EvalImprovementObjective::Progression,
        EvalImprovementObjective::ParentComparison,
        EvalImprovementObjective::BaselineComparison,
        EvalImprovementObjective::VariantComparison,
    ];
    cycle_value(&items, objective, forward)
}

fn cycle_delta_kind(
    kind: bcode_eval_models::EvalImprovementDeltaKind,
    forward: bool,
) -> bcode_eval_models::EvalImprovementDeltaKind {
    use bcode_eval_models::EvalImprovementDeltaKind as Kind;
    let items = [
        Kind::SystemPromptOverlay,
        Kind::ToolDescriptionOverlay,
        Kind::ToolBehaviorPatch,
        Kind::AgentProfileOverlay,
        Kind::PermissionPolicyOverlay,
        Kind::ModelChange,
        Kind::EvalCaseChange,
        Kind::JudgeChange,
        Kind::ScoringChange,
        Kind::Mixed,
    ];
    cycle_value(&items, kind, forward)
}

fn cycle_risk(
    risk: bcode_eval_models::EvalImprovementRisk,
    forward: bool,
) -> bcode_eval_models::EvalImprovementRisk {
    use bcode_eval_models::EvalImprovementRisk as Risk;
    cycle_value(&[Risk::Low, Risk::Medium, Risk::High], risk, forward)
}

fn campaign_run_choices(data: &EvalCampaignData) -> Result<(Vec<RecordRunChoice>, usize), String> {
    let runs_root = data
        .campaign_dir
        .parent()
        .and_then(std::path::Path::parent)
        .map_or_else(
            || PathBuf::from("target/bcode-evals/runs"),
            |root| root.join("runs"),
        );
    let used_runs = data
        .generations
        .iter()
        .filter_map(|generation| generation.run_dir.as_ref())
        .collect::<std::collections::BTreeSet<_>>();
    let all_runs = discover_runs(&runs_root)
        .into_iter()
        .filter(|run| run.suite_id == data.campaign.suite_id)
        .collect::<Vec<_>>();
    if all_runs.is_empty() {
        return Err(format!(
            "no runs found for suite {}",
            data.campaign.suite_id
        ));
    }
    let mut choices = vec![RecordRunChoice {
        label: "no run".to_string(),
        run_dir: None,
    }];
    choices.extend(all_runs.iter().map(|run| RecordRunChoice {
        label: if used_runs.contains(&run.run_dir) {
            format!("{} (duplicate)", run.run_id)
        } else {
            run.run_id.clone()
        },
        run_dir: Some(run.run_dir.clone()),
    }));
    let index = choices
        .iter()
        .position(|choice| {
            choice
                .run_dir
                .as_ref()
                .is_some_and(|run_dir| !used_runs.contains(run_dir))
        })
        .unwrap_or(0);
    Ok((choices, index))
}

fn campaign_parent_choices(data: &EvalCampaignData) -> Vec<RecordParentChoice> {
    data.generations
        .iter()
        .map(|generation| {
            let mut labels = Vec::new();
            if generation.id == data.campaign.baseline_generation_id {
                labels.push("baseline");
            }
            if data.campaign.latest_generation_id.as_ref() == Some(&generation.id) {
                labels.push("latest");
            }
            if data.campaign.best_generation_id.as_ref() == Some(&generation.id) {
                labels.push("best");
            }
            let suffix = if labels.is_empty() {
                String::new()
            } else {
                format!(" ({})", labels.join(", "))
            };
            RecordParentChoice {
                label: format!("{}{suffix}", generation.id),
                parent_id: Some(generation.id.clone()),
            }
        })
        .collect()
}

fn suite_choices_from_runs(runs: &[EvalRunSummary]) -> Vec<StartCampaignSuiteChoice> {
    let historical = runs
        .iter()
        .filter_map(|run| EvalRunData::load(&run.run_dir).ok())
        .filter_map(|data| data.result.manifest.suite_path)
        .collect::<Vec<_>>();
    discover_suites(historical)
        .into_iter()
        .filter(|suite| suite.error.is_none())
        .map(|suite| {
            let baseline = runs.iter().find(|run| run.suite_id == suite.suite_id);
            StartCampaignSuiteChoice {
                suite_id: suite.suite_id,
                suite_path: suite.suite_path,
                baseline_run: baseline.map(|run| run.run_dir.clone()),
                run_id: baseline.map(|run| run.run_id.clone()),
            }
        })
        .collect()
}

fn comma_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn comma_paths(value: &str) -> Vec<PathBuf> {
    comma_values(value).into_iter().map(PathBuf::from).collect()
}

fn optional_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn optional_path(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn empty_label(value: &str) -> &str {
    if value.trim().is_empty() {
        "none"
    } else {
        value
    }
}

fn cycle_index(len: usize, current: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (current + 1) % len
    } else {
        current.checked_sub(1).unwrap_or(len - 1)
    }
}

fn cycle_value<T: Copy + PartialEq>(items: &[T], current: T, forward: bool) -> T {
    let index = items.iter().position(|item| *item == current).unwrap_or(0);
    let next = if forward {
        (index + 1) % items.len()
    } else {
        index.checked_sub(1).unwrap_or(items.len() - 1)
    };
    items[next]
}

const fn objective_label(objective: EvalImprovementObjective) -> &'static str {
    match objective {
        EvalImprovementObjective::Progression => "Track improvement over time",
        EvalImprovementObjective::ParentComparison => "Compare each generation to parent",
        EvalImprovementObjective::BaselineComparison => "Compare against baseline",
        EvalImprovementObjective::VariantComparison => "Compare candidates",
    }
}

const fn risk_label(risk: bcode_eval_models::EvalImprovementRisk) -> &'static str {
    match risk {
        bcode_eval_models::EvalImprovementRisk::Low => "Low",
        bcode_eval_models::EvalImprovementRisk::Medium => "Medium",
        bcode_eval_models::EvalImprovementRisk::High => "High",
    }
}

const fn delta_kind_label(kind: bcode_eval_models::EvalImprovementDeltaKind) -> &'static str {
    use bcode_eval_models::EvalImprovementDeltaKind as Kind;
    match kind {
        Kind::Baseline => "Baseline",
        Kind::SystemPromptOverlay => "System prompt guidance",
        Kind::SystemPromptPatch => "System prompt patch",
        Kind::ToolDescriptionOverlay => "Tool description/schema",
        Kind::ToolSchemaPatch => "Tool schema patch",
        Kind::ToolBehaviorPatch => "Tool implementation",
        Kind::AgentProfileOverlay => "Agent profile",
        Kind::PermissionPolicyOverlay => "Permission policy",
        Kind::ModelChange => "Model/settings",
        Kind::EvalCaseChange => "Eval case",
        Kind::JudgeChange => "Judge/scoring",
        Kind::ScoringChange => "Scoring change",
        Kind::Mixed => "Mixed / not sure",
    }
}

fn wizard_actions(primary: &'static str) -> Vec<ActionButton> {
    vec![
        ActionButton::new(
            primary,
            match primary {
                "create" => "Create",
                "run" => "Run",
                "record" => "Record",
                "promote" => "Promote",
                "reject" => "Reject",
                _ => "Confirm",
            },
        ),
        ActionButton::new("cancel", "Cancel"),
    ]
}

const fn eval_modal_theme() -> ModalTheme {
    ModalTheme::dark(ACCENT)
}

fn unique_campaign_id(root: &std::path::Path, base: &str) -> String {
    let mut candidate = sanitize_id(base);
    if !root.join(&candidate).exists() {
        return candidate;
    }
    for index in 2..1000_u16 {
        let next = format!("{}-{index}", sanitize_id(base));
        if !root.join(&next).exists() {
            return next;
        }
        candidate = next;
    }
    candidate
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[derive(Debug, Clone, Copy)]
enum OverviewRow {
    Run(usize),
    Campaign(usize),
}

struct CampaignRunCompletion {
    run_id: String,
    generation: EvalImprovementGeneration,
}

/// Eval improvement campaign viewer surface.
pub struct EvalCampaignViewerSurface {
    data: EvalCampaignData,
    generation_state: TableState,
    action_state: ActionRowState,
    selected_run_viewer: Option<EvalRunViewerSurface>,
    detail_view: Option<EvalGenerationDetailSurface>,
    active_wizard: Option<EvalWizard>,
    run_task: Option<Receiver<Result<CampaignRunCompletion, String>>>,
    view_mode: CampaignViewMode,
    metric_names: Vec<String>,
    metric_index: usize,
    status: String,
    table_area: Rect,
    action_area: Rect,
    surface_area: Rect,
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
        let view_mode = CampaignViewMode::from_objective(data.campaign.objective);
        let metric_names = campaign_metric_names(&data);
        Ok(Self {
            data,
            generation_state: TableState::new(Some(0)),
            action_state: ActionRowState::new(),
            selected_run_viewer: None,
            detail_view: None,
            active_wizard: None,
            run_task: None,
            view_mode,
            metric_names,
            metric_index: 0,
            status,
            table_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
            surface_area: Rect::new(0, 0, 0, 0),
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

    fn open_selected_detail(&mut self) {
        let Some(generation) = self.selected_generation().cloned() else {
            self.status = "select a generation first".to_string();
            return;
        };
        self.detail_view = Some(EvalGenerationDetailSurface::new(
            self.data.clone(),
            generation.id,
        ));
    }

    fn cycle_view_mode(&mut self) {
        self.view_mode = self.view_mode.next();
        self.status = format!("view mode: {}", self.view_mode.label());
    }

    fn run_next(&mut self, host: &dyn PluginTuiHost) {
        if self.run_task.is_some() {
            self.status = "a campaign suite is already running".to_string();
            return;
        }
        let parent_id = self
            .selected_generation()
            .map(|generation| generation.id.clone())
            .or_else(|| self.data.campaign.latest_generation_id.clone());
        let run_options = bcode_eval::EvalRunOptions {
            suite_path: self.data.campaign.suite_path.clone(),
            output_root: self
                .data
                .campaign_dir
                .parent()
                .and_then(std::path::Path::parent)
                .map_or_else(
                    || PathBuf::from("target/bcode-evals/runs"),
                    |root| root.join("runs"),
                ),
            run_id: None,
        };
        let campaign = self.data.campaign_dir.clone();
        let (sender, receiver) = std_mpsc::channel();
        host.spawn_blocking(Box::new(move || {
            let result = bcode_eval::run_suite(&run_options)
                .map_err(|error| error.to_string())
                .and_then(|run| {
                    let run_id = run.manifest.run_id.clone();
                    let options = bcode_eval::EvalImprovementRecordOptions {
                        campaign,
                        parent_id,
                        branch: "main".to_string(),
                        delta_kind: bcode_eval_models::EvalImprovementDeltaKind::Mixed,
                        summary: format!("Rerun {run_id}"),
                        run: Some(run.manifest.output_dir),
                        patch: None,
                        overlays: Vec::new(),
                        affected_files: Vec::new(),
                        affected_surfaces: Vec::new(),
                        expected_impact: None,
                        risk: bcode_eval_models::EvalImprovementRisk::Low,
                        rationale: Some("Executed and recorded from the campaign TUI.".to_string()),
                    };
                    bcode_eval::record_improvement_generation(options)
                        .map(|generation| CampaignRunCompletion { run_id, generation })
                        .map_err(|error| format!("run completed but recording failed: {error}"))
                });
            let _ = sender.send(result);
        }));
        self.run_task = Some(receiver);
        self.status = "running campaign suite in background...".to_string();
    }

    fn poll_run_task(&mut self) -> PluginTuiAction {
        let Some(receiver) = self.run_task.as_ref() else {
            return PluginTuiAction::None;
        };
        match receiver.try_recv() {
            Ok(Ok(completion)) => {
                self.run_task = None;
                self.status = format!(
                    "completed run {} as generation {}",
                    completion.run_id, completion.generation.id
                );
                if let Ok(data) = EvalCampaignData::load(&self.data.campaign_dir) {
                    self.data = data;
                }
                PluginTuiAction::Redraw
            }
            Ok(Err(error)) => {
                self.run_task = None;
                self.status = error;
                PluginTuiAction::Redraw
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.run_task = None;
                self.status = "campaign run task disconnected".to_string();
                PluginTuiAction::Redraw
            }
            Err(std_mpsc::TryRecvError::Empty) => PluginTuiAction::None,
        }
    }

    fn decide_selected_generation(
        &mut self,
        status: bcode_eval_models::EvalImprovementVerdictStatus,
    ) {
        let Some(generation) = self.selected_generation().cloned() else {
            self.status = "select a generation first".to_string();
            return;
        };
        match EvalWizard::decide_generation(&self.data, &generation, status) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn record_generation(&mut self) {
        match EvalWizard::record_generation_for_campaign(&self.data, self.selected_generation()) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn complete_wizard(&mut self, completion: EvalWizardCompletion) {
        match completion {
            EvalWizardCompletion::RecordGeneration(options) => {
                match bcode_eval::record_improvement_generation(*options) {
                    Ok(generation) => {
                        self.status = format!("recorded generation {}", generation.id);
                        if let Ok(data) = EvalCampaignData::load(&self.data.campaign_dir) {
                            self.data = data;
                        }
                    }
                    Err(error) => self.status = format!("failed to record generation: {error}"),
                }
            }
            EvalWizardCompletion::DecideGeneration(options) => {
                match bcode_eval::decide_improvement_generation(options.as_ref()) {
                    Ok(generation) => {
                        self.status = format!(
                            "generation {} is now {:?}",
                            generation.id, generation.verdict.status
                        );
                        if let Ok(data) = EvalCampaignData::load(&self.data.campaign_dir) {
                            self.data = data;
                        }
                    }
                    Err(error) => self.status = format!("failed to update generation: {error}"),
                }
            }
            EvalWizardCompletion::RunSuite(_) => {
                self.status = "run suite is only available from eval home".to_string();
            }
            EvalWizardCompletion::StartCampaign(_) => {
                self.status = "start campaign is only available from eval home".to_string();
            }
        }
    }

    fn handle_action(&mut self, action: &str, host: &dyn PluginTuiHost) -> PluginTuiAction {
        match action {
            "details" => self.open_selected_detail(),
            "open-run" => self.open_selected_run(),
            "view-mode" => self.cycle_view_mode(),
            "record-generation" => self.record_generation(),
            "run-next" => self.run_next(host),
            "promote" => self.decide_selected_generation(
                bcode_eval_models::EvalImprovementVerdictStatus::Promoted,
            ),
            "reject" => self.decide_selected_generation(
                bcode_eval_models::EvalImprovementVerdictStatus::Rejected,
            ),
            "help" => {
                self.active_wizard = Some(EvalWizard::help(
                    "Campaign Help",
                    vec![
                        "Details: inspect the selected generation",
                        "Open Run: inspect its eval run",
                        "Record: add a generation from an unused run",
                        "Run Next: execute the suite and record against the selection",
                        "Promote/Reject: record a reviewed terminal decision",
                        "View: cycle progression and comparison lenses",
                    ],
                ));
            }
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
        self.surface_area = area;
        if let Some(viewer) = self.selected_run_viewer.as_mut() {
            viewer.render(area, frame);
            return;
        }
        if let Some(detail) = self.detail_view.as_mut() {
            detail.render(area, frame);
            return;
        }
        if let Some(wizard) = self.active_wizard.as_mut() {
            wizard.render(area, frame);
            return;
        }
        render_header(
            area,
            frame,
            &format!(
                "Eval Campaign: {} — {}",
                self.data.campaign.id,
                self.view_mode.label()
            ),
            &self.status,
        );
        let body = body_area(area);
        let (table_area, action_area, status_area) = split_body_actions(body);
        self.table_area = inset_top(table_area, 1);
        self.action_area = action_area;
        let (columns, rows, title) = match self.view_mode {
            CampaignViewMode::CaseHistory => {
                let (columns, rows) = case_history_table(&self.data);
                (columns, rows, "Case history across generations")
            }
            CampaignViewMode::Metrics => {
                let metric = self.metric_names.get(self.metric_index).map(String::as_str);
                let (columns, rows) = campaign_metric_table(&self.data, metric);
                (columns, rows, "Dynamic campaign metric")
            }
            _ => (
                campaign_columns(self.view_mode),
                campaign_rows(&self.data, self.view_mode),
                "Generation timeline",
            ),
        };
        render_panel_title(table_area, frame, title);
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
            "Enter details. O opens run. V cycles progression/comparison view. Esc returns.",
        );
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(wizard) = self.active_wizard.as_mut() {
            match wizard.handle_event(self.surface_area, event) {
                EvalWizardOutcome::Continue => return PluginTuiAction::None,
                EvalWizardOutcome::Redraw => return PluginTuiAction::Redraw,
                EvalWizardOutcome::Cancel => {
                    self.active_wizard = None;
                    return PluginTuiAction::Redraw;
                }
                EvalWizardOutcome::Complete(completion) => {
                    self.active_wizard = None;
                    self.complete_wizard(completion);
                    return PluginTuiAction::Redraw;
                }
            }
        }
        if let Some(viewer) = self.selected_run_viewer.as_mut() {
            let action = viewer.handle_event(event, host);
            if matches!(action, PluginTuiAction::Close { .. }) {
                self.selected_run_viewer = None;
                return PluginTuiAction::Redraw;
            }
            return action;
        }
        if let Some(detail) = self.detail_view.as_mut() {
            let action = detail.handle_event(event, host);
            if matches!(action, PluginTuiAction::Close { .. }) {
                self.detail_view = None;
                return PluginTuiAction::Redraw;
            }
            return action;
        }
        let (columns, rows) = match self.view_mode {
            CampaignViewMode::CaseHistory => case_history_table(&self.data),
            CampaignViewMode::Metrics => campaign_metric_table(
                &self.data,
                self.metric_names.get(self.metric_index).map(String::as_str),
            ),
            _ => (
                campaign_columns(self.view_mode),
                campaign_rows(&self.data, self.view_mode),
            ),
        };
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
            ActionRowOutcome::Activated { id, .. } => return self.handle_action(&id, host),
            outcome if outcome.needs_redraw() => return PluginTuiAction::Redraw,
            _ => {}
        }
        if let Event::Key(stroke) = event {
            match stroke.key {
                KeyCode::Enter => {
                    self.open_selected_detail();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('o') => {
                    self.open_selected_run();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('v') => {
                    self.cycle_view_mode();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('m') => {
                    self.metric_index =
                        cycle_index(self.metric_names.len(), self.metric_index, true);
                    if let Some(metric) = self.metric_names.get(self.metric_index) {
                        self.status = format!("metric: {metric}");
                    }
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('r') => return self.handle_action("record-generation", host),
                KeyCode::Char('u') => return self.handle_action("run-next", host),
                KeyCode::Char('p') => return self.handle_action("promote", host),
                KeyCode::Char('x') => return self.handle_action("reject", host),
                KeyCode::Char('?') => return self.handle_action("help", host),
                KeyCode::Char('q') | KeyCode::Escape => {
                    return PluginTuiAction::Close { outcome: None };
                }
                _ => {}
            }
        }
        PluginTuiAction::None
    }

    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        let task_action = self.poll_run_task();
        if !matches!(task_action, PluginTuiAction::None) {
            return task_action;
        }
        if let Some(detail) = self.detail_view.as_mut() {
            return detail.poll(host);
        }
        PluginTuiAction::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CampaignViewMode {
    Progression,
    ParentDelta,
    BaselineDelta,
    Comparison,
    CaseHistory,
    Metrics,
}

impl CampaignViewMode {
    const fn from_objective(objective: EvalImprovementObjective) -> Self {
        match objective {
            EvalImprovementObjective::Progression => Self::Progression,
            EvalImprovementObjective::ParentComparison => Self::ParentDelta,
            EvalImprovementObjective::BaselineComparison => Self::BaselineDelta,
            EvalImprovementObjective::VariantComparison => Self::Comparison,
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Progression => Self::ParentDelta,
            Self::ParentDelta => Self::BaselineDelta,
            Self::BaselineDelta => Self::Comparison,
            Self::Comparison => Self::CaseHistory,
            Self::CaseHistory => Self::Metrics,
            Self::Metrics => Self::Progression,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Progression => "Progression",
            Self::ParentDelta => "Parent Δ",
            Self::BaselineDelta => "Baseline Δ",
            Self::Comparison => "Comparison",
            Self::CaseHistory => "Case History",
            Self::Metrics => "Metrics",
        }
    }
}

/// Generation detail surface shown from a campaign timeline.
pub struct EvalGenerationDetailSurface {
    data: EvalCampaignData,
    generation_id: String,
    tab_state: TabBarState,
    action_state: ActionRowState,
    selected_run_viewer: Option<EvalRunViewerSurface>,
    active_wizard: Option<EvalWizard>,
    rerun_task: Option<Receiver<Result<CampaignRunCompletion, String>>>,
    tab_area: Rect,
    action_area: Rect,
    surface_area: Rect,
    status: String,
}

impl EvalGenerationDetailSurface {
    fn new(data: EvalCampaignData, generation_id: String) -> Self {
        Self {
            status: "inspect generation details, delta, metrics, or run".to_string(),
            data,
            generation_id,
            tab_state: TabBarState::new(Some(0)),
            action_state: ActionRowState::new(),
            selected_run_viewer: None,
            active_wizard: None,
            rerun_task: None,
            tab_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
            surface_area: Rect::new(0, 0, 0, 0),
        }
    }

    fn open_branch_wizard(&mut self) {
        let generation = self.generation().cloned();
        match generation.as_ref().map_or_else(
            || Err("generation not found".to_string()),
            |generation| EvalWizard::record_generation_for_campaign(&self.data, Some(generation)),
        ) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn open_decision_wizard(&mut self, status: bcode_eval_models::EvalImprovementVerdictStatus) {
        let generation = self.generation().cloned();
        match generation.as_ref().map_or_else(
            || Err("generation not found".to_string()),
            |generation| EvalWizard::decide_generation(&self.data, generation, status),
        ) {
            Ok(wizard) => self.active_wizard = Some(wizard),
            Err(error) => self.status = error,
        }
    }

    fn complete_wizard(&mut self, completion: EvalWizardCompletion) {
        match completion {
            EvalWizardCompletion::RecordGeneration(options) => {
                match bcode_eval::record_improvement_generation(*options) {
                    Ok(generation) => {
                        self.status = format!("recorded branch {}", generation.id);
                    }
                    Err(error) => self.status = format!("branch failed: {error}"),
                }
            }
            EvalWizardCompletion::DecideGeneration(options) => {
                match bcode_eval::decide_improvement_generation(options.as_ref()) {
                    Ok(generation) => {
                        self.status = format!(
                            "generation {} is now {:?}",
                            generation.id, generation.verdict.status
                        );
                    }
                    Err(error) => self.status = format!("decision failed: {error}"),
                }
            }
            _ => {}
        }
        if let Ok(data) = EvalCampaignData::load(&self.data.campaign_dir) {
            self.data = data;
        }
    }

    fn generation(&self) -> Option<&EvalImprovementGeneration> {
        self.data
            .generations
            .iter()
            .find(|generation| generation.id == self.generation_id)
    }

    fn open_run(&mut self) {
        let Some(generation) = self.generation() else {
            self.status = "generation not found".to_string();
            return;
        };
        let Some(run_dir) = generation.run_dir.clone() else {
            self.status = "generation has no run".to_string();
            return;
        };
        match EvalRunViewerSurface::load(run_dir) {
            Ok(viewer) => self.selected_run_viewer = Some(viewer),
            Err(error) => self.status = format!("failed to open run: {error}"),
        }
    }

    fn rerun(&mut self, host: &dyn PluginTuiHost) {
        if self.rerun_task.is_some() {
            self.status = "this generation is already rerunning".to_string();
            return;
        }
        let run_options = bcode_eval::EvalRunOptions {
            suite_path: self.data.campaign.suite_path.clone(),
            output_root: self
                .data
                .campaign_dir
                .parent()
                .and_then(std::path::Path::parent)
                .map_or_else(
                    || PathBuf::from("target/bcode-evals/runs"),
                    |root| root.join("runs"),
                ),
            run_id: None,
        };
        let campaign = self.data.campaign_dir.clone();
        let parent_id = self.generation_id.clone();
        let branch = self.generation().map_or_else(
            || "main".to_string(),
            |generation| generation.branch.clone(),
        );
        let (sender, receiver) = std_mpsc::channel();
        host.spawn_blocking(Box::new(move || {
            let result = bcode_eval::run_suite(&run_options)
                .map_err(|error| error.to_string())
                .and_then(|run| {
                    let run_id = run.manifest.run_id.clone();
                    let options = bcode_eval::EvalImprovementRecordOptions {
                        campaign,
                        parent_id: Some(parent_id),
                        branch,
                        delta_kind: bcode_eval_models::EvalImprovementDeltaKind::Mixed,
                        summary: format!("Rerun {run_id}"),
                        run: Some(run.manifest.output_dir),
                        patch: None,
                        overlays: Vec::new(),
                        affected_files: Vec::new(),
                        affected_surfaces: Vec::new(),
                        expected_impact: None,
                        risk: bcode_eval_models::EvalImprovementRisk::Low,
                        rationale: Some("Rerun from generation detail TUI.".to_string()),
                    };
                    bcode_eval::record_improvement_generation(options)
                        .map(|generation| CampaignRunCompletion { run_id, generation })
                        .map_err(|error| format!("rerun recording failed: {error}"))
                });
            let _ = sender.send(result);
        }));
        self.rerun_task = Some(receiver);
        self.status = "rerunning suite in background...".to_string();
    }

    fn poll_rerun_task(&mut self) -> PluginTuiAction {
        let Some(receiver) = self.rerun_task.as_ref() else {
            return PluginTuiAction::None;
        };
        match receiver.try_recv() {
            Ok(Ok(completion)) => {
                self.rerun_task = None;
                self.status = format!(
                    "completed run {} as generation {}",
                    completion.run_id, completion.generation.id
                );
                if let Ok(data) = EvalCampaignData::load(&self.data.campaign_dir) {
                    self.data = data;
                }
                PluginTuiAction::Redraw
            }
            Ok(Err(error)) => {
                self.rerun_task = None;
                self.status = error;
                PluginTuiAction::Redraw
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.rerun_task = None;
                self.status = "rerun task disconnected".to_string();
                PluginTuiAction::Redraw
            }
            Err(std_mpsc::TryRecvError::Empty) => PluginTuiAction::None,
        }
    }

    fn selected_tab(&self) -> GenerationDetailTab {
        GenerationDetailTab::from_index(self.tab_state.selected().unwrap_or(0))
    }

    fn render_summary(&self, area: Rect, frame: &mut Frame<'_>) {
        let Some(generation) = self.generation() else {
            render_lines(area, frame, &[Line::from("generation not found")]);
            return;
        };
        let current = self.data.generation_run(generation);
        let parent = self
            .data
            .parent_generation(generation)
            .and_then(|parent| self.data.generation_run(parent));
        let baseline = self
            .data
            .generations
            .iter()
            .find(|candidate| candidate.id == self.data.campaign.baseline_generation_id)
            .and_then(|baseline| self.data.generation_run(baseline));
        let mut lines = vec![
            Line::from_spans(vec![Span::styled(
                format!("Generation {}", generation.id),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!("Change: {}", generation.delta.summary)),
            Line::from(format!("Kind: {:?}", generation.delta.kind)),
            Line::from(format!("Risk: {:?}", generation.delta.risk)),
            Line::from(format!("Verdict: {:?}", generation.verdict.status)),
            Line::from(""),
            Line::from("Current performance"),
        ];
        lines.extend(metric_summary_lines(current.as_ref()));
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "vs parent: {} pass, {} score",
            pass_delta_label(parent.as_ref(), current.as_ref()),
            score_delta_label(parent.as_ref(), current.as_ref())
        )));
        lines.push(Line::from(format!(
            "vs baseline: {} pass, {} score",
            pass_delta_label(baseline.as_ref(), current.as_ref()),
            score_delta_label(baseline.as_ref(), current.as_ref())
        )));
        render_lines(area, frame, &lines);
    }

    fn render_delta(&self, area: Rect, frame: &mut Frame<'_>) {
        let Some(generation) = self.generation() else {
            render_lines(area, frame, &[Line::from("generation not found")]);
            return;
        };
        let mut lines = vec![
            Line::from_spans(vec![Span::styled(
                "What changed",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!("Summary: {}", generation.delta.summary)),
            Line::from(format!("Kind: {:?}", generation.delta.kind)),
            Line::from(format!("Risk: {:?}", generation.delta.risk)),
        ];
        if let Some(rationale) = &generation.delta.rationale {
            lines.push(Line::from(""));
            lines.push(Line::from("Rationale:"));
            lines.extend(
                rationale
                    .lines()
                    .map(|line| Line::from(format!("  {line}"))),
            );
        }
        if !generation.delta.affected_surfaces.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("Affected surfaces:"));
            lines.extend(
                generation
                    .delta
                    .affected_surfaces
                    .iter()
                    .map(|surface| Line::from(format!("  * {surface}"))),
            );
        }
        if !generation.delta.affected_files.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("Affected files:"));
            lines.extend(
                generation
                    .delta
                    .affected_files
                    .iter()
                    .map(|path| Line::from(format!("  * {}", path.display()))),
            );
        }
        if let Some(patch_path) = &generation.delta.patch_path {
            lines.push(Line::from(""));
            lines.push(Line::from(format!("Patch: {}", patch_path.display())));
        }
        if !generation.delta.overlay_paths.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("Overlays:"));
            lines.extend(
                generation
                    .delta
                    .overlay_paths
                    .iter()
                    .map(|path| Line::from(format!("  * {}", path.display()))),
            );
        }
        render_lines(area, frame, &lines);
    }

    fn render_metrics(&self, area: Rect, frame: &mut Frame<'_>) {
        let Some(generation) = self.generation() else {
            render_lines(area, frame, &[Line::from("generation not found")]);
            return;
        };
        let current = self.data.generation_run(generation);
        let parent = self
            .data
            .parent_generation(generation)
            .and_then(|parent| self.data.generation_run(parent));
        let rows = metric_comparison_rows(parent.as_ref(), current.as_ref());
        render_eval_table(
            frame,
            area,
            &metric_columns(),
            &rows,
            &TableState::new(None),
        );
    }
}

impl PluginTuiSurface for EvalGenerationDetailSurface {
    fn id(&self) -> &'static str {
        "bcode.eval-generation-detail"
    }

    fn title(&self) -> &'static str {
        "Eval Generation"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.surface_area = area;
        if let Some(wizard) = self.active_wizard.as_mut() {
            wizard.render(area, frame);
            return;
        }
        if let Some(viewer) = self.selected_run_viewer.as_mut() {
            viewer.render(area, frame);
            return;
        }
        render_header(
            area,
            frame,
            &format!("Generation {}", self.generation_id),
            &self.status,
        );
        let tabs = generation_detail_tabs();
        self.tab_area = Rect::new(
            area.x,
            area.y.saturating_add(TITLE_HEIGHT),
            area.width,
            TAB_HEIGHT,
        );
        TabBar::new(&tabs)
            .styles(eval_tab_styles())
            .render(self.tab_area, &self.tab_state, frame);
        let body = Rect::new(
            area.x,
            area.y.saturating_add(TITLE_HEIGHT + TAB_HEIGHT),
            area.width,
            area.height.saturating_sub(TITLE_HEIGHT + TAB_HEIGHT),
        );
        let (content_area, action_area, status_area) = split_body_actions(body);
        self.action_area = action_area;
        match self.selected_tab() {
            GenerationDetailTab::Summary => self.render_summary(content_area, frame),
            GenerationDetailTab::Delta => self.render_delta(content_area, frame),
            GenerationDetailTab::Metrics => self.render_metrics(content_area, frame),
        }
        themed_action_row(&generation_detail_actions()).render_state(
            action_area,
            &self.action_state,
            frame,
        );
        render_status(
            status_area,
            frame,
            "Tab switches panes. O opens run. Esc returns.",
        );
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(wizard) = self.active_wizard.as_mut() {
            match wizard.handle_event(self.surface_area, event) {
                EvalWizardOutcome::Continue => return PluginTuiAction::None,
                EvalWizardOutcome::Redraw => return PluginTuiAction::Redraw,
                EvalWizardOutcome::Cancel => {
                    self.active_wizard = None;
                    return PluginTuiAction::Redraw;
                }
                EvalWizardOutcome::Complete(completion) => {
                    self.active_wizard = None;
                    self.complete_wizard(completion);
                    return PluginTuiAction::Redraw;
                }
            }
        }
        if let Some(viewer) = self.selected_run_viewer.as_mut() {
            let action = viewer.handle_event(event, host);
            if matches!(action, PluginTuiAction::Close { .. }) {
                self.selected_run_viewer = None;
                return PluginTuiAction::Redraw;
            }
            return action;
        }
        let tabs = generation_detail_tabs();
        match TabBar::new(&tabs).styles(eval_tab_styles()).handle_event(
            self.tab_area,
            &mut self.tab_state,
            event,
        ) {
            TabBarOutcome::Selected(_) | TabBarOutcome::Redraw => return PluginTuiAction::Redraw,
            TabBarOutcome::Ignored => {}
        }
        match themed_action_row(&generation_detail_actions()).handle_event(
            self.action_area,
            &mut self.action_state,
            event,
        ) {
            ActionRowOutcome::Activated { id, .. } => match id.as_str() {
                "open-run" => {
                    self.open_run();
                    return PluginTuiAction::Redraw;
                }
                "rerun" => {
                    self.rerun(host);
                    return PluginTuiAction::Redraw;
                }
                "branch" => self.open_branch_wizard(),
                "promote" => self.open_decision_wizard(
                    bcode_eval_models::EvalImprovementVerdictStatus::Promoted,
                ),
                "reject" => self.open_decision_wizard(
                    bcode_eval_models::EvalImprovementVerdictStatus::Rejected,
                ),
                "back" => return PluginTuiAction::Close { outcome: None },
                _ => {}
            },
            outcome if outcome.needs_redraw() => return PluginTuiAction::Redraw,
            _ => {}
        }
        if let Event::Key(stroke) = event {
            match stroke.key {
                KeyCode::Tab => {
                    cycle_tab(&mut self.tab_state, generation_detail_tabs().len());
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('o') => {
                    self.open_run();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('r') => {
                    self.rerun(host);
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('b') => {
                    self.open_branch_wizard();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('p' | 'x') => {
                    let status = if stroke.key == KeyCode::Char('p') {
                        bcode_eval_models::EvalImprovementVerdictStatus::Promoted
                    } else {
                        bcode_eval_models::EvalImprovementVerdictStatus::Rejected
                    };
                    self.open_decision_wizard(status);
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

    fn poll(&mut self, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        self.poll_rerun_task()
    }
}

#[derive(Debug, Clone, Copy)]
enum GenerationDetailTab {
    Summary,
    Delta,
    Metrics,
}

impl GenerationDetailTab {
    const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Delta,
            2 => Self::Metrics,
            _ => Self::Summary,
        }
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
    artifact: Option<(String, String, bool, bool)>,
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
            self.artifact = Some((
                artifact.title,
                artifact.text,
                artifact.truncated,
                artifact.binary,
            ));
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
        let Some((title, text, truncated, binary)) = &self.artifact else {
            render_panel_title(area, frame, "Artifact viewer");
            render_status(
                inset_top(area, 1),
                frame,
                "Select a repetition, then use Diff, Transcript, or Tool Calls.",
            );
            return;
        };
        render_panel_title(area, frame, title);
        let notice = match (*binary, *truncated) {
            (true, true) => "Binary artifact; preview metadata is truncated",
            (true, false) => "Binary artifact; text preview unavailable",
            (false, true) => "Artifact preview truncated at 1 MiB",
            (false, false) => "",
        };
        let content_offset = u16::from(!notice.is_empty());
        if !notice.is_empty() {
            frame.write_line_with_fallback_style(
                Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
                &Line::from_spans(vec![Span::styled(notice, Style::new().fg(WARNING))]),
                Style::new().bg(PANEL),
            );
        }
        for (row, line) in text
            .lines()
            .skip(self.artifact_scroll)
            .take(usize::from(
                area.height.saturating_sub(1).saturating_sub(content_offset),
            ))
            .enumerate()
        {
            let y = area
                .y
                .saturating_add(1)
                .saturating_add(content_offset)
                .saturating_add(usize_to_u16(row));
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

fn case_history_table(data: &EvalCampaignData) -> (Vec<TableColumn<'_>>, Vec<TableRow>) {
    let generations = data
        .generations
        .iter()
        .map(|generation| generation.id.as_str())
        .collect::<Vec<_>>();
    let mut columns = vec![TableColumn::new("Case").flex(2)];
    columns.extend(generations.iter().map(|generation| {
        TableColumn::new(generation)
            .fixed(14)
            .align(TableAlign::Right)
    }));
    let rows = campaign_case_history(data)
        .into_iter()
        .map(|row| {
            let cells = generations.iter().map(|generation| {
                row.cells
                    .iter()
                    .find(|cell| cell.generation_id == *generation)
                    .map_or_else(
                        || "—".to_string(),
                        |cell| {
                            let score = cell
                                .score
                                .map_or_else(|| "—".to_string(), |score| format!("{score:.2}"));
                            format!(
                                "{:.0}%/{score}/{}",
                                cell.pass_rate * 100.0,
                                cell.repetitions
                            )
                        },
                    )
            });
            string_row(std::iter::once(row.case_id).chain(cells).collect())
        })
        .collect();
    (columns, rows)
}

fn campaign_metric_table(
    data: &EvalCampaignData,
    metric: Option<&str>,
) -> (Vec<TableColumn<'static>>, Vec<TableRow>) {
    let columns = vec![
        TableColumn::new("Generation").fixed(14),
        TableColumn::new("Metric").flex(2),
        TableColumn::new("Average")
            .fixed(16)
            .align(TableAlign::Right),
    ];
    let rows = data
        .generations
        .iter()
        .map(|generation| {
            let value = metric
                .and_then(|name| data.generation_run(generation).map(|run| (name, run)))
                .and_then(|(name, run)| run_avg_measurement(&run.result, name));
            string_row(vec![
                generation.id.clone(),
                metric.unwrap_or("no measurements").to_string(),
                value.map_or_else(|| "—".to_string(), |value| format!("{value:.3}")),
            ])
        })
        .collect();
    (columns, rows)
}

fn campaign_columns<'a>(mode: CampaignViewMode) -> Vec<TableColumn<'a>> {
    match mode {
        CampaignViewMode::Progression => vec![
            TableColumn::new("Gen").fixed(8),
            TableColumn::new("Change").flex(3),
            TableColumn::new("Pass").fixed(8).align(TableAlign::Right),
            TableColumn::new("Score").fixed(8).align(TableAlign::Right),
            TableColumn::new("Cost").fixed(9).align(TableAlign::Right),
            TableColumn::new("Tokens")
                .fixed(10)
                .align(TableAlign::Right),
            TableColumn::new("Latency")
                .fixed(10)
                .align(TableAlign::Right),
            TableColumn::new("Δ Parent")
                .fixed(10)
                .align(TableAlign::Right),
            TableColumn::new("Verdict").fixed(12),
        ],
        CampaignViewMode::ParentDelta | CampaignViewMode::BaselineDelta => vec![
            TableColumn::new("Gen").fixed(8),
            TableColumn::new("Change").flex(3),
            TableColumn::new("Pass Δ").fixed(9).align(TableAlign::Right),
            TableColumn::new("Score Δ")
                .fixed(9)
                .align(TableAlign::Right),
            TableColumn::new("Cost Δ").fixed(9).align(TableAlign::Right),
            TableColumn::new("Tokens Δ")
                .fixed(10)
                .align(TableAlign::Right),
            TableColumn::new("Latency Δ")
                .fixed(11)
                .align(TableAlign::Right),
            TableColumn::new("Verdict").fixed(12),
        ],
        CampaignViewMode::CaseHistory | CampaignViewMode::Metrics => vec![
            TableColumn::new("Item").flex(2),
            TableColumn::new("Value").flex(1),
        ],
        CampaignViewMode::Comparison => vec![
            TableColumn::new("Gen").fixed(8),
            TableColumn::new("Parent").fixed(8),
            TableColumn::new("Change").flex(3),
            TableColumn::new("Parent Pass")
                .fixed(12)
                .align(TableAlign::Right),
            TableColumn::new("Current Pass")
                .fixed(13)
                .align(TableAlign::Right),
            TableColumn::new("Score Δ")
                .fixed(9)
                .align(TableAlign::Right),
            TableColumn::new("Verdict").fixed(12),
        ],
    }
}

fn campaign_rows(data: &EvalCampaignData, mode: CampaignViewMode) -> Vec<TableRow> {
    data.generations
        .iter()
        .map(|generation| match mode {
            CampaignViewMode::Progression => campaign_progression_row(data, generation),
            CampaignViewMode::ParentDelta => {
                campaign_delta_row(data, generation, data.parent_generation(generation))
            }
            CampaignViewMode::BaselineDelta => {
                let baseline = data
                    .generations
                    .iter()
                    .find(|candidate| candidate.id == data.campaign.baseline_generation_id);
                campaign_delta_row(data, generation, baseline)
            }
            CampaignViewMode::Comparison => campaign_comparison_row(data, generation),
            CampaignViewMode::CaseHistory | CampaignViewMode::Metrics => {
                string_row(vec![generation.id.clone(), "—".to_string()])
            }
        })
        .collect()
}

fn campaign_progression_row(
    data: &EvalCampaignData,
    generation: &EvalImprovementGeneration,
) -> TableRow {
    let current = data.generation_run(generation);
    let parent = data
        .parent_generation(generation)
        .and_then(|parent| data.generation_run(parent));
    string_row(vec![
        generation.id.clone(),
        generation.delta.summary.clone(),
        current.as_ref().map_or_else(
            || "—".to_string(),
            |run| format_percent(run_pass_rate(&run.result)),
        ),
        current.as_ref().map_or_else(
            || "—".to_string(),
            |run| format_score(run_best_score(&run.result)),
        ),
        current
            .as_ref()
            .and_then(|run| run_avg_measurement(&run.result, "estimated_cost_usd"))
            .map_or_else(|| "—".to_string(), format_cost),
        current
            .as_ref()
            .and_then(|run| run_avg_measurement(&run.result, "total_tokens"))
            .map_or_else(|| "—".to_string(), format_number),
        current
            .as_ref()
            .and_then(|run| run_avg_measurement(&run.result, "wall_time_ms"))
            .map_or_else(|| "—".to_string(), format_duration_ms),
        pass_delta(parent.as_ref(), current.as_ref())
            .map_or_else(|| "—".to_string(), format_signed_percent),
        format!("{:?}", generation.verdict.status),
    ])
}

fn campaign_delta_row(
    data: &EvalCampaignData,
    generation: &EvalImprovementGeneration,
    comparison: Option<&EvalImprovementGeneration>,
) -> TableRow {
    let current = data.generation_run(generation);
    let compare = comparison.and_then(|generation| data.generation_run(generation));
    string_row(vec![
        generation.id.clone(),
        generation.delta.summary.clone(),
        pass_delta(compare.as_ref(), current.as_ref())
            .map_or_else(|| "—".to_string(), format_signed_percent),
        score_delta(compare.as_ref(), current.as_ref())
            .map_or_else(|| "—".to_string(), format_signed),
        metric_delta(compare.as_ref(), current.as_ref(), "estimated_cost_usd")
            .map_or_else(|| "—".to_string(), format_signed_cost),
        metric_delta(compare.as_ref(), current.as_ref(), "total_tokens")
            .map_or_else(|| "—".to_string(), format_signed_number),
        metric_delta(compare.as_ref(), current.as_ref(), "wall_time_ms")
            .map_or_else(|| "—".to_string(), format_signed_duration),
        format!("{:?}", generation.verdict.status),
    ])
}

fn campaign_comparison_row(
    data: &EvalCampaignData,
    generation: &EvalImprovementGeneration,
) -> TableRow {
    let current = data.generation_run(generation);
    let parent_generation = data.parent_generation(generation);
    let parent = parent_generation.and_then(|parent| data.generation_run(parent));
    string_row(vec![
        generation.id.clone(),
        generation
            .parent_id
            .clone()
            .unwrap_or_else(|| "—".to_string()),
        generation.delta.summary.clone(),
        parent.as_ref().map_or_else(
            || "—".to_string(),
            |run| format_percent(run_pass_rate(&run.result)),
        ),
        current.as_ref().map_or_else(
            || "—".to_string(),
            |run| format_percent(run_pass_rate(&run.result)),
        ),
        score_delta(parent.as_ref(), current.as_ref())
            .map_or_else(|| "—".to_string(), format_signed),
        format!("{:?}", generation.verdict.status),
    ])
}

fn campaign_actions() -> Vec<ActionButton> {
    vec![
        ActionButton::new("details", "Enter Details"),
        ActionButton::new("open-run", "O Open Run"),
        ActionButton::new("view-mode", "V View"),
        ActionButton::new("record-generation", "R Record"),
        ActionButton::new("run-next", "U Run Next"),
        ActionButton::new("promote", "P Promote"),
        ActionButton::new("reject", "X Reject"),
        ActionButton::new("help", "? Help"),
        ActionButton::new("refresh", "Refresh"),
        ActionButton::new("back", "Esc Back"),
    ]
}

fn generation_detail_tabs() -> Vec<TabItem<'static>> {
    vec![
        TabItem::new("summary", "Summary"),
        TabItem::new("delta", "Delta"),
        TabItem::new("metrics", "Metrics"),
    ]
}

fn generation_detail_actions() -> Vec<ActionButton> {
    vec![
        ActionButton::new("open-run", "O Open Run"),
        ActionButton::new("rerun", "R Rerun"),
        ActionButton::new("branch", "B Branch"),
        ActionButton::new("promote", "P Promote"),
        ActionButton::new("reject", "X Reject"),
        ActionButton::new("back", "Esc Back"),
    ]
}

fn metric_columns<'a>() -> Vec<TableColumn<'a>> {
    vec![
        TableColumn::new("Metric").flex(2),
        TableColumn::new("Parent")
            .fixed(12)
            .align(TableAlign::Right),
        TableColumn::new("Current")
            .fixed(12)
            .align(TableAlign::Right),
        TableColumn::new("Delta").fixed(12).align(TableAlign::Right),
    ]
}

fn metric_comparison_rows(
    parent: Option<&EvalRunData>,
    current: Option<&EvalRunData>,
) -> Vec<TableRow> {
    vec![
        metric_comparison_required_row(
            "Pass rate",
            parent,
            current,
            pass_value,
            format_percent,
            format_signed_percent,
        ),
        metric_comparison_required_row(
            "Score",
            parent,
            current,
            score_value,
            format_score,
            format_signed,
        ),
        metric_comparison_row(
            "Cost",
            parent,
            current,
            cost_value,
            format_cost,
            format_signed_cost,
        ),
        metric_comparison_row(
            "Tokens",
            parent,
            current,
            token_value,
            format_number,
            format_signed_number,
        ),
        metric_comparison_row(
            "Latency",
            parent,
            current,
            latency_value,
            format_duration_ms,
            format_signed_duration,
        ),
    ]
}

fn metric_comparison_row(
    label: &str,
    parent: Option<&EvalRunData>,
    current: Option<&EvalRunData>,
    value: fn(&EvalRunData) -> Option<f64>,
    format: fn(f64) -> String,
    format_delta: fn(f64) -> String,
) -> TableRow {
    let parent_value = parent.and_then(value);
    let current_value = current.and_then(value);
    string_row(vec![
        label.to_string(),
        parent_value.map_or_else(|| "—".to_string(), format),
        current_value.map_or_else(|| "—".to_string(), format),
        parent_value.zip(current_value).map_or_else(
            || "—".to_string(),
            |(parent, current)| format_delta(current - parent),
        ),
    ])
}

fn metric_comparison_required_row(
    label: &str,
    parent: Option<&EvalRunData>,
    current: Option<&EvalRunData>,
    value: fn(&EvalRunData) -> f64,
    format: fn(f64) -> String,
    format_delta: fn(f64) -> String,
) -> TableRow {
    let parent_value = parent.map(value);
    let current_value = current.map(value);
    string_row(vec![
        label.to_string(),
        parent_value.map_or_else(|| "—".to_string(), format),
        current_value.map_or_else(|| "—".to_string(), format),
        parent_value.zip(current_value).map_or_else(
            || "—".to_string(),
            |(parent, current)| format_delta(current - parent),
        ),
    ])
}

fn metric_summary_lines(current: Option<&EvalRunData>) -> Vec<Line> {
    vec![
        Line::from(format!(
            "  Pass: {}",
            current.map_or_else(|| "—".to_string(), |run| format_percent(pass_value(run)))
        )),
        Line::from(format!(
            "  Score: {}",
            current.map_or_else(|| "—".to_string(), |run| format_score(score_value(run)))
        )),
        Line::from(format!(
            "  Cost: {}",
            current
                .and_then(cost_value)
                .map_or_else(|| "—".to_string(), format_cost)
        )),
        Line::from(format!(
            "  Tokens: {}",
            current
                .and_then(token_value)
                .map_or_else(|| "—".to_string(), format_number)
        )),
        Line::from(format!(
            "  Latency: {}",
            current
                .and_then(latency_value)
                .map_or_else(|| "—".to_string(), format_duration_ms)
        )),
    ]
}

fn pass_value(run: &EvalRunData) -> f64 {
    run_pass_rate(&run.result)
}

fn score_value(run: &EvalRunData) -> f64 {
    run_best_score(&run.result)
}

fn cost_value(run: &EvalRunData) -> Option<f64> {
    run_avg_measurement(&run.result, "estimated_cost_usd")
}

fn token_value(run: &EvalRunData) -> Option<f64> {
    run_avg_measurement(&run.result, "total_tokens")
}

fn latency_value(run: &EvalRunData) -> Option<f64> {
    run_avg_measurement(&run.result, "wall_time_ms")
}

fn pass_delta(parent: Option<&EvalRunData>, current: Option<&EvalRunData>) -> Option<f64> {
    Some(pass_value(current?) - pass_value(parent?))
}

fn score_delta(parent: Option<&EvalRunData>, current: Option<&EvalRunData>) -> Option<f64> {
    Some(score_value(current?) - score_value(parent?))
}

fn pass_delta_label(parent: Option<&EvalRunData>, current: Option<&EvalRunData>) -> String {
    pass_delta(parent, current).map_or_else(|| "—".to_string(), format_signed_percent)
}

fn score_delta_label(parent: Option<&EvalRunData>, current: Option<&EvalRunData>) -> String {
    score_delta(parent, current).map_or_else(|| "—".to_string(), format_signed)
}

fn render_lines(area: Rect, frame: &mut Frame<'_>, lines: &[Line]) {
    for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        frame.write_line_with_fallback_style(
            Rect::new(
                area.x,
                area.y.saturating_add(usize_to_u16(row)),
                area.width,
                1,
            ),
            line,
            Style::new().bg(PANEL),
        );
    }
}

fn cycle_tab(state: &mut TabBarState, len: usize) {
    let next = (state.selected().unwrap_or(0) + 1) % len.max(1);
    state.set_selected(Some(next));
}

fn format_percent(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn format_signed_percent(value: f64) -> String {
    if value >= 0.0 {
        format!("+{:.1}%", value * 100.0)
    } else {
        format!("{:.1}%", value * 100.0)
    }
}

fn format_score(value: f64) -> String {
    format!("{value:.3}")
}

fn format_cost(value: f64) -> String {
    format!("${value:.3}")
}

fn format_signed_cost(value: f64) -> String {
    if value >= 0.0 {
        format!("+${value:.3}")
    } else {
        format!("-${:.3}", value.abs())
    }
}

fn format_signed_duration(value: f64) -> String {
    if value >= 0.0 {
        format!("+{}", format_duration_ms(value))
    } else {
        format!("-{}", format_duration_ms(value.abs()))
    }
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
        ActionButton::new("run-suite", "U Run Suite"),
        ActionButton::new("new-campaign", "N New Campaign"),
        ActionButton::new("start-campaign", "S Start Campaign"),
        ActionButton::new("attach-run", "A Attach Run"),
        ActionButton::new("refresh", "R Refresh"),
        ActionButton::new("help", "? Help"),
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

#[cfg(test)]
mod interaction_tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::Event;
    use bmux_tui::prelude::Rect;

    use super::{handle_input_box, input_text, text_state};

    #[test]
    fn focused_wizard_input_accepts_plain_text() {
        let mut state = text_state("");
        let area = Rect::new(0, 0, 40, 4);

        for character in "query data".chars() {
            let key = if character == ' ' {
                KeyCode::Space
            } else {
                KeyCode::Char(character)
            };
            assert!(handle_input_box(
                area,
                &mut state,
                &Event::Key(KeyStroke::simple(key)),
                true,
            ));
        }

        assert_eq!(input_text(&state), "query data");
    }
}
