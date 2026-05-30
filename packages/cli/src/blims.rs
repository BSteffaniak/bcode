#![allow(clippy::module_name_repetitions)]

use super::{CliError, attach_session, ensure_server_running, print_service_response};
use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bcode_worktree_models::{WorktreeBaseRef, WorktreeCreateRequest, WorktreeCreateResponse};
use bmux_keyboard::KeyCode;
use bmux_tui::layout::centered;
use bmux_tui::prelude::{
    Border, Color, Constraint, Direction, Event, Frame, Insets, Line, Modifier, Panel, Point, Rect,
    Size, Span, Style, Terminal, Text, TextBlock, TextWrap, Widget, event_from_crossterm, split,
};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Subcommand)]
pub enum BlimsCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Create {
        #[arg(long)]
        json: bool,
    },
    Pause {
        #[arg(long)]
        json: bool,
    },
    Resume {
        #[arg(long)]
        json: bool,
    },
    Shutdown {
        #[arg(long)]
        json: bool,
    },
    Inspect {
        agent_id: String,
        #[arg(long)]
        json: bool,
    },
    Hire {
        agent_id: String,
        name: String,
        role: String,
        #[arg(long, default_value = "ceo-nook")]
        room_id: String,
        #[arg(long)]
        json: bool,
    },
    Suspend {
        agent_id: String,
        #[arg(long)]
        json: bool,
    },
    Fire {
        agent_id: String,
        #[arg(long)]
        json: bool,
    },
    Permissions {
        agent_id: String,
        #[arg(long)]
        json: bool,
    },
    SetPermission {
        agent_id: String,
        #[arg(long)]
        bcode_agent_id: Option<String>,
        #[arg(long)]
        bash: Option<String>,
        #[arg(long)]
        read: Option<String>,
        #[arg(long)]
        write: Option<String>,
        #[arg(long)]
        edit: Option<String>,
        #[arg(long)]
        external_directory: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Agents {
        #[arg(long)]
        json: bool,
    },
    World {
        #[arg(long)]
        json: bool,
    },
    WorldTemplates {
        #[arg(long)]
        json: bool,
    },
    SelectWorld {
        template_id: String,
        #[arg(long)]
        json: bool,
    },
    Enter,
    Talk {
        agent_id: String,
    },
    AiWork {
        #[command(subcommand)]
        command: BlimsAiWorkCommand,
    },
    Task {
        #[command(subcommand)]
        command: BlimsTaskCommand,
    },
    Artifact {
        #[command(subcommand)]
        command: BlimsArtifactCommand,
    },
    Proposal {
        #[command(subcommand)]
        command: BlimsProposalCommand,
    },
    Initiative {
        #[command(subcommand)]
        command: BlimsInitiativeCommand,
    },
    Guidance {
        #[command(subcommand)]
        command: BlimsGuidanceCommand,
    },
    Report {
        #[arg(long)]
        json: bool,
    },
    DepartmentReport {
        department_id: String,
        #[arg(long)]
        json: bool,
    },
    AgentReport {
        agent_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlimsAiWorkCommand {
    List {
        #[arg(long, default_value_t = 25)]
        limit: u64,
        #[arg(long)]
        json: bool,
    },
    Start {
        work_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlimsInitiativeCommand {
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Inspect {
        initiative_id: String,
        #[arg(long)]
        json: bool,
    },
    SetGuidance {
        initiative_id: String,
        guidance: String,
        #[arg(long, default_value = "strong")]
        strength: String,
        #[arg(long)]
        json: bool,
    },
    Pause {
        initiative_id: String,
        #[arg(long)]
        json: bool,
    },
    Resume {
        initiative_id: String,
        #[arg(long)]
        json: bool,
    },
    PlanPrompt {
        initiative_id: String,
    },
    Plan {
        initiative_id: String,
    },
    ImportPlan {
        initiative_id: String,
        plan: String,
        #[arg(long)]
        file: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlimsGuidanceCommand {
    Set {
        guidance: String,
        #[arg(long, default_value = "strong")]
        strength: String,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlimsTaskCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Inspect {
        task_id: String,
        #[arg(long)]
        json: bool,
    },
    Work {
        task_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlimsArtifactCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Inspect {
        artifact_id: String,
        #[arg(long)]
        json: bool,
    },
    Apply {
        artifact_id: String,
        #[arg(long)]
        yes: bool,
    },
    Create {
        kind: String,
        title: String,
        #[arg(long)]
        initiative_id: String,
        #[arg(long)]
        payload: Option<String>,
        #[arg(long)]
        file: bool,
        #[arg(long)]
        json: bool,
    },
    Approve {
        artifact_id: String,
        #[arg(long)]
        json: bool,
    },
    Reject {
        artifact_id: String,
        #[arg(long)]
        json: bool,
    },
    Defer {
        artifact_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlimsProposalCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Inspect {
        proposal_id: String,
        #[arg(long)]
        json: bool,
    },
    MarkReady {
        proposal_id: String,
        #[arg(long)]
        json: bool,
    },
    Approve {
        proposal_id: String,
        #[arg(long)]
        json: bool,
    },
    Reject {
        proposal_id: String,
        #[arg(long)]
        json: bool,
    },
    Defer {
        proposal_id: String,
        #[arg(long)]
        json: bool,
    },
    Patch {
        proposal_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BlimsWorkspaceRequest {
    working_directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BlimsInitiativeCreateRequest {
    working_directory: PathBuf,
    title: String,
    description: Option<String>,
    priority: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BlimsGuidanceSetRequest {
    working_directory: PathBuf,
    guidance: String,
    strength: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BlimsInitiativeGuidanceRequest {
    working_directory: PathBuf,
    initiative_id: String,
    guidance: String,
    strength: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BlimsInitiativePlanPromptRequest {
    working_directory: PathBuf,
    initiative_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BlimsInitiativeImportPlanRequest {
    working_directory: PathBuf,
    initiative_id: String,
    plan: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsCompanyStatus {
    state: String,
    message: String,
    daemon_connected: bool,
    state_root: PathBuf,
    database_path: PathBuf,
    lifecycle_status: String,
    daemon_namespace: String,
    daemon_metadata_path: PathBuf,
    compatibility: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsAgentSnapshot {
    id: String,
    name: String,
    role: String,
    status: String,
    room_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BlimsAgentPermission {
    agent_id: String,
    bcode_agent_id: String,
    bash: String,
    read: String,
    write: String,
    edit: String,
    external_directory: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsRoomSnapshot {
    id: String,
    name: String,
    purpose: String,
    room_kind: String,
    productivity_modifier: i64,
    x: i64,
    y: i64,
    symbol: String,
    color: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsWorldSnapshot {
    theme: String,
    template_id: String,
    width: i64,
    height: i64,
    player_name: String,
    rooms: Vec<BlimsRoomSnapshot>,
    agents: Vec<BlimsAgentSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsWorldInteraction {
    id: String,
    label: String,
    command: String,
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsWorldTemplateSummary {
    id: String,
    name: String,
    description: String,
    rooms: usize,
    flavor: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsAvailableInteractions {
    room_id: String,
    interactions: Vec<BlimsWorldInteraction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsInitiativeSummary {
    id: String,
    title: String,
    description: String,
    status: String,
    priority: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsGuidanceSummary {
    id: String,
    guidance: String,
    strength: String,
    active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsTaskSummary {
    id: String,
    initiative_id: String,
    title: String,
    description: String,
    status: String,
    assigned_agent_id: String,
    rationale: String,
    priority: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsArtifactSummary {
    id: String,
    initiative_id: String,
    kind: String,
    title: String,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsArtifactDetail {
    id: String,
    initiative_id: String,
    kind: String,
    title: String,
    status: String,
    payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsAgentTalkPrompt {
    agent_id: String,
    conversation_id: String,
    prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsTaskWorkPrompt {
    task_id: String,
    agent_id: String,
    prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsPreparedAiWorkItem {
    id: String,
    operation_id: String,
    kind: String,
    agent_id: String,
    task_id: Option<String>,
    prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsActivityItem {
    title: String,
    body: String,
    actor_id: String,
    severity: String,
    action_hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsCeoInboxItem {
    id: String,
    kind: String,
    title: String,
    summary: String,
    priority: i64,
    actor_id: String,
    action_label: String,
    action_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BlimsWorkProposalSummary {
    id: String,
    task_id: String,
    initiative_id: String,
    agent_id: String,
    session_id: String,
    worktree_path: String,
    branch: String,
    status: String,
    summary: String,
    validation_notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsPlanningPrompt {
    initiative_id: String,
    prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsMorningReport {
    title: String,
    bullets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsFocusedReport {
    title: String,
    bullets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsEventSummary {
    kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsDashboardProjection {
    initiatives: Vec<BlimsInitiativeSummary>,
    tasks: Vec<BlimsTaskSummary>,
    proposals: Vec<BlimsWorkProposalSummary>,
    artifacts: Vec<BlimsArtifactSummary>,
    guidance: Vec<BlimsGuidanceSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsCommandSubmitResponse {
    operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsWorldSelection {
    world: BlimsWorldSnapshot,
    report: BlimsMorningReport,
    interactions: BlimsAvailableInteractions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsCeoDashboardState {
    initiatives: Vec<BlimsInitiativeSummary>,
    tasks: Vec<BlimsTaskSummary>,
    proposals: Vec<BlimsWorkProposalSummary>,
    artifacts: Vec<BlimsArtifactSummary>,
    guidance: Vec<BlimsGuidanceSummary>,
    selected_section: usize,
    selected_item: usize,
    status: String,
}

impl BlimsCeoDashboardState {
    const SECTIONS: usize = 4;

    const fn loading(status: String) -> Self {
        Self {
            initiatives: Vec::new(),
            tasks: Vec::new(),
            proposals: Vec::new(),
            artifacts: Vec::new(),
            guidance: Vec::new(),
            selected_section: 0,
            selected_item: 0,
            status,
        }
    }

    const fn select_next_section(&mut self) {
        self.selected_section = (self.selected_section + 1) % Self::SECTIONS;
        self.selected_item = 0;
    }

    fn select_previous_section(&mut self) {
        self.selected_section = self
            .selected_section
            .checked_sub(1)
            .unwrap_or(Self::SECTIONS - 1);
        self.selected_item = 0;
    }

    fn select_next_item(&mut self) {
        let count = self.selected_count();
        if count > 0 {
            self.selected_item = (self.selected_item + 1) % count;
        }
    }

    fn select_previous_item(&mut self) {
        let count = self.selected_count();
        if count > 0 {
            self.selected_item = self
                .selected_item
                .checked_sub(1)
                .unwrap_or_else(|| count.saturating_sub(1));
        }
    }

    fn selected_count(&self) -> usize {
        match self.selected_section {
            0 => self.tasks.len().min(8),
            1 => self.proposals.len().min(8),
            2 => self.artifacts.len().min(8),
            3 => self
                .guidance
                .len()
                .min(3)
                .saturating_add(self.initiatives.len().min(5)),
            _ => 0,
        }
    }

    fn selected_proposal_id(&self) -> Option<String> {
        (self.selected_section == 1)
            .then(|| {
                self.proposals
                    .get(self.selected_item)
                    .map(|proposal| proposal.id.clone())
            })
            .flatten()
    }

    fn selected_artifact_id(&self) -> Option<String> {
        (self.selected_section == 2)
            .then(|| {
                self.artifacts
                    .get(self.selected_item)
                    .map(|artifact| artifact.id.clone())
            })
            .flatten()
    }

    fn update_proposal(&mut self, proposal: BlimsWorkProposalSummary) {
        if let Some(existing) = self
            .proposals
            .iter_mut()
            .find(|existing| existing.id == proposal.id)
        {
            *existing = proposal;
        }
    }

    fn update_artifact(&mut self, artifact: BlimsArtifactDetail) {
        if let Some(existing) = self
            .artifacts
            .iter_mut()
            .find(|existing| existing.id == artifact.id)
        {
            existing.status = artifact.status;
            existing.title = artifact.title;
            existing.kind = artifact.kind;
            existing.initiative_id = artifact.initiative_id;
        }
    }

    fn selected_action_hint(&self) -> String {
        match self.selected_section {
            0 => "Task selected. Use task work from CLI for now; in-world launch is next."
                .to_string(),
            1 => "Proposal selected. Press a approve, x reject, f defer.".to_string(),
            2 => "Artifact selected. Press a approve, x reject, f defer.".to_string(),
            3 => "Guidance/initiative selected. Whiteboard editing is next.".to_string(),
            _ => "Nothing selected.".to_string(),
        }
    }
}

pub async fn handle_blims_command(command: BlimsCommand) -> Result<(), CliError> {
    ensure_server_running().await?;
    match command {
        BlimsCommand::Status { json } => print_blims_status(json).await?,
        BlimsCommand::Create { json } => create_blims_company(json).await?,
        BlimsCommand::Pause { json } => {
            print_company_lifecycle_update("company.pause", "Blims company paused", json).await?;
        }
        BlimsCommand::Resume { json } => {
            print_company_lifecycle_update("company.resume", "Blims company resumed", json).await?;
        }
        BlimsCommand::Shutdown { json } => {
            print_company_lifecycle_update("company.shutdown", "Blims company shut down", json)
                .await?;
        }
        BlimsCommand::Inspect { .. }
        | BlimsCommand::Hire { .. }
        | BlimsCommand::Suspend { .. }
        | BlimsCommand::Fire { .. }
        | BlimsCommand::Permissions { .. }
        | BlimsCommand::SetPermission { .. } => handle_blims_agent_command(command).await?,
        BlimsCommand::Agents { json } => {
            let response = call_blims_service("agent.list", blims_workspace_payload()?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                let agents = decode_blims_response::<Vec<BlimsAgentSnapshot>>(response)?;
                for agent in agents {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        agent.id, agent.name, agent.role, agent.room_id, agent.status
                    );
                }
            }
        }
        BlimsCommand::World { json } => {
            let response = call_blims_service("world.snapshot", blims_workspace_payload()?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                let world = decode_blims_response::<BlimsWorldSnapshot>(response)?;
                print_blims_world(&world);
            }
        }
        BlimsCommand::WorldTemplates { json } => print_world_templates(json).await?,
        BlimsCommand::SelectWorld { template_id, json } => {
            select_world_template(template_id, json).await?;
        }
        BlimsCommand::Enter => enter_blims_office().await?,
        BlimsCommand::Talk { agent_id } => start_blims_agent_talk_cli(agent_id).await?,
        BlimsCommand::AiWork { command } => handle_blims_ai_work_command(command).await?,
        BlimsCommand::Task { command } => handle_blims_task_command(command).await?,
        BlimsCommand::Artifact { command } => handle_blims_artifact_command(command).await?,
        BlimsCommand::Proposal { command } => handle_blims_proposal_command(command).await?,
        BlimsCommand::Initiative { command } => handle_blims_initiative_command(command).await?,
        BlimsCommand::Guidance { command } => handle_blims_guidance_command(command).await?,
        BlimsCommand::Report { json } => {
            let response = call_blims_service("report.morning", blims_workspace_payload()?).await?;
            print_report_response::<BlimsMorningReport>(response, json)?;
        }
        BlimsCommand::DepartmentReport {
            department_id,
            json,
        } => {
            let request = serde_json::json!({
                "working_directory": std::env::current_dir()?,
                "department_id": department_id,
            });
            let response =
                call_blims_service("report.department", serde_json::to_vec(&request)?).await?;
            print_report_response::<BlimsFocusedReport>(response, json)?;
        }
        BlimsCommand::AgentReport { agent_id, json } => {
            let request = serde_json::json!({
                "working_directory": std::env::current_dir()?,
                "agent_id": agent_id,
            });
            let response =
                call_blims_service("report.agent", serde_json::to_vec(&request)?).await?;
            print_report_response::<BlimsFocusedReport>(response, json)?;
        }
    }
    Ok(())
}

async fn handle_blims_agent_command(command: BlimsCommand) -> Result<(), CliError> {
    match command {
        BlimsCommand::Inspect { agent_id, json } => {
            print_agent_service_result("agent.inspect", &agent_id, json).await?;
        }
        BlimsCommand::Hire {
            agent_id,
            name,
            role,
            room_id,
            json,
        } => {
            hire_blims_agent(agent_id, name, role, room_id, json).await?;
        }
        BlimsCommand::Suspend { agent_id, json } => {
            print_agent_service_result("agent.suspend", &agent_id, json).await?;
        }
        BlimsCommand::Fire { agent_id, json } => {
            print_agent_service_result("agent.fire", &agent_id, json).await?;
        }
        BlimsCommand::Permissions { agent_id, json } => {
            print_agent_permission(&agent_id, json).await?;
        }
        BlimsCommand::SetPermission {
            agent_id,
            bcode_agent_id,
            bash,
            read,
            write,
            edit,
            external_directory,
            json,
        } => {
            set_agent_permission(
                agent_id,
                bcode_agent_id,
                bash,
                read,
                write,
                edit,
                external_directory,
                json,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn enter_blims_office() -> Result<(), CliError> {
    run_blims_tui().await
}

async fn run_blims_tui() -> Result<(), CliError> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(CliError::Blims(
            "`bcode blims enter` needs an interactive terminal; stdout/stdin is not a TTY"
                .to_string(),
        ));
    }

    let mut guard = BlimsTerminalGuard::enter()?;
    let mut terminal = Terminal::new(
        guard
            .writer_mut()
            .ok_or_else(|| std::io::Error::other("terminal guard writer unavailable"))?,
        blims_terminal_area()?,
    );
    terminal.draw(render_blims_loading)?;
    let mut app = BlimsTuiApp::load().await?;
    if should_show_world_picker().await? {
        app.show_world_picker = true;
    }
    terminal.draw(|frame| app.render(frame))?;
    loop {
        let mut dirty = false;
        let mut should_quit = false;
        while let Some(event) = blims_poll_event(Duration::from_millis(0))? {
            dirty = true;
            if handle_blims_tui_event(&mut app, event).await? {
                should_quit = true;
                break;
            }
        }
        if should_quit {
            break;
        }
        if let Some(event) = blims_poll_event(Duration::from_millis(16))? {
            dirty = true;
            if handle_blims_tui_event(&mut app, event).await? {
                break;
            }
        }
        dirty |= app.poll_background_jobs();
        dirty |= app.animate_agents();
        app.schedule_background_jobs();
        terminal.resize(blims_terminal_area()?);
        if dirty {
            terminal.draw(|frame| app.render(frame))?;
        }
    }
    let _stdout = guard.leave()?;
    Ok(())
}

fn render_blims_loading(frame: &mut Frame<'_>) {
    let area = frame.area();
    frame.fill(area, " ", Style::new().bg(Color::Rgb(12, 10, 18)));
    let modal = centered(area, Size::new(52, 7));
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightMagenta)))
        .title(" Blims ")
        .background(Style::new().bg(Color::Rgb(19, 16, 30)));
    panel.render(modal, frame);
    TextBlock::new(Text::from_lines(vec![
        Line::from_spans(vec![Span::styled(
            "✨ Opening the office…",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::raw("Loading world, agents, and reports."),
    ]))
    .style(
        Style::new()
            .bg(Color::Rgb(19, 16, 30))
            .fg(Color::BrightWhite),
    )
    .render(panel.inner_area(modal).inset(Insets::all(1)), frame);
}

async fn should_show_world_picker() -> Result<bool, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "limit": 25_u64,
    });
    let response = call_blims_service("event.list", serde_json::to_vec(&request)?).await?;
    let events = decode_blims_response::<Vec<BlimsEventSummary>>(response)?;
    Ok(!events
        .iter()
        .any(|event| event.kind == "world.starter_office_selected"))
}

fn blims_terminal_area() -> Result<Rect, CliError> {
    let (width, height) = crossterm::terminal::size()?;
    Ok(Rect::new(0, 0, width, height))
}

struct BlimsTerminalGuard {
    stdout: Option<std::io::Stdout>,
    active: bool,
}

impl BlimsTerminalGuard {
    fn enter() -> Result<Self, CliError> {
        let mut stdout = std::io::stdout();
        crossterm::terminal::enable_raw_mode()?;
        if let Err(error) = crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::terminal::DisableLineWrap,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        ) {
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self {
            stdout: Some(stdout),
            active: true,
        })
    }

    const fn writer_mut(&mut self) -> Option<&mut std::io::Stdout> {
        self.stdout.as_mut()
    }

    fn leave(mut self) -> Result<std::io::Stdout, CliError> {
        self.leave_inner()?;
        self.active = false;
        self.stdout
            .take()
            .ok_or_else(|| CliError::Blims("Blims terminal writer already taken".to_string()))
    }

    fn leave_inner(&mut self) -> Result<(), CliError> {
        if let Some(stdout) = &mut self.stdout {
            crossterm::execute!(
                stdout,
                crossterm::style::ResetColor,
                crossterm::cursor::Show,
                crossterm::terminal::EnableLineWrap,
                crossterm::terminal::LeaveAlternateScreen
            )?;
            stdout.flush()?;
        }
        crossterm::terminal::disable_raw_mode()?;
        Ok(())
    }
}

impl Drop for BlimsTerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.leave_inner();
        }
    }
}

fn blims_poll_event(timeout: Duration) -> Result<Option<Event>, CliError> {
    if !crossterm::event::poll(timeout)? {
        return Ok(None);
    }
    Ok(event_from_crossterm(crossterm::event::read()?))
}

async fn handle_blims_tui_event(app: &mut BlimsTuiApp, event: Event) -> Result<bool, CliError> {
    match event {
        Event::Resize(_) | Event::Mouse(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {}
        Event::Paste(text) => {
            app.status = format!(
                "Pasted {} bytes (command entry is coming next).",
                text.len()
            );
        }
        Event::Key(stroke) => {
            if handle_blims_tui_conversation_key(app, stroke.key).await? {
                return Ok(false);
            }
            if handle_blims_tui_dashboard_key(app, stroke.key).await? {
                return Ok(false);
            }
            if matches!(stroke.key, KeyCode::Escape | KeyCode::Char('q')) {
                return Ok(true);
            }
            if handle_blims_tui_picker_key(app, stroke.key) {
                return Ok(false);
            }
            match stroke.key {
                KeyCode::Char('?') => app.show_help = !app.show_help,
                KeyCode::Char('d') => app.open_ceo_dashboard(),
                KeyCode::Char('w') => app.show_world_picker = true,
                KeyCode::Char('r') => {
                    app.report = load_blims_report().await?;
                    app.status = "Morning report refreshed.".to_string();
                }
                KeyCode::Char('e') | KeyCode::Enter => app.activate_primary_action().await?,
                KeyCode::Tab => app.select_next_interaction(),
                KeyCode::Char('n') => app.select_next_inbox_item(),
                KeyCode::Char('N') => app.select_previous_inbox_item(),
                KeyCode::Char('t') => app.open_nearest_agent_conversation().await?,
                KeyCode::Char('h') | KeyCode::Left => app.move_player_by(-1, 0),
                KeyCode::Char('l') | KeyCode::Right => app.move_player_by(1, 0),
                KeyCode::Char('k') | KeyCode::Up => app.move_player_by(0, -1),
                KeyCode::Char('j') | KeyCode::Down => app.move_player_by(0, 1),
                KeyCode::Char('H') => app.move_previous().await?,
                KeyCode::Char('L') => app.move_next().await?,
                _ => {}
            }
        }
    }
    Ok(false)
}

async fn handle_blims_tui_conversation_key(
    app: &mut BlimsTuiApp,
    key: KeyCode,
) -> Result<bool, CliError> {
    let BlimsTuiMode::Conversation(conversation) = &mut app.mode else {
        return Ok(false);
    };
    match key {
        KeyCode::Escape => {
            app.mode = BlimsTuiMode::Office;
            app.status = "Returned to office.".to_string();
        }
        KeyCode::Enter => {
            let text = conversation.input.trim().to_string();
            if text.is_empty() {
                conversation.status = "Type a message first.".to_string();
            } else {
                BcodeClient::default_endpoint()
                    .send_user_message(conversation.handle.session, text.clone())
                    .await?;
                let _operation = submit_blims_command(&serde_json::json!({
                    "type": "record_conversation_message",
                    "conversation_id": conversation.handle.conversation_ref,
                    "agent_id": conversation.handle.agent_id,
                    "speaker": "ceo",
                    "message": text,
                }))
                .await?;
                conversation.input.clear();
                conversation.status = "Message sent.".to_string();
                refresh_conversation_transcript(conversation).await?;
            }
        }
        KeyCode::Backspace => {
            conversation.input.pop();
        }
        KeyCode::Char(value) => conversation.input.push(value),
        _ => {}
    }
    Ok(true)
}

async fn handle_blims_tui_dashboard_key(
    app: &mut BlimsTuiApp,
    key: KeyCode,
) -> Result<bool, CliError> {
    let BlimsTuiMode::Dashboard(dashboard) = &mut app.mode else {
        return Ok(false);
    };
    match key {
        KeyCode::Escape | KeyCode::Char('d') => {
            app.mode = BlimsTuiMode::Office;
            app.jobs.dashboard_load = None;
            app.status = "Returned to office.".to_string();
        }
        KeyCode::Char('r') => {
            dashboard.status = "Refreshing CEO dashboard…".to_string();
            if app.jobs.dashboard_load.is_none() {
                app.jobs.dashboard_load = Some(BlimsBackgroundJob::DashboardLoad(
                    spawn_blims_background(|| Box::pin(load_ceo_dashboard())),
                ));
            }
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => dashboard.select_next_section(),
        KeyCode::Left | KeyCode::Char('h') => dashboard.select_previous_section(),
        KeyCode::Down | KeyCode::Char('j') => dashboard.select_next_item(),
        KeyCode::Up | KeyCode::Char('k') => dashboard.select_previous_item(),
        KeyCode::Enter => dashboard.status = dashboard.selected_action_hint(),
        KeyCode::Char('a') => {
            if let Some(proposal_id) = dashboard.selected_proposal_id() {
                let proposal =
                    update_blims_proposal_status("proposal.approve", proposal_id).await?;
                dashboard.update_proposal(proposal);
                dashboard.status = "Proposal approved.".to_string();
            } else if let Some(artifact_id) = dashboard.selected_artifact_id() {
                let artifact =
                    update_blims_artifact_status("artifact.approve", artifact_id).await?;
                dashboard.update_artifact(artifact);
                dashboard.status = "Artifact approved.".to_string();
            } else {
                dashboard.status = "Select a proposal or artifact to approve.".to_string();
            }
        }
        KeyCode::Char('x') => {
            if let Some(proposal_id) = dashboard.selected_proposal_id() {
                let proposal = update_blims_proposal_status("proposal.reject", proposal_id).await?;
                dashboard.update_proposal(proposal);
                dashboard.status = "Proposal rejected.".to_string();
            } else if let Some(artifact_id) = dashboard.selected_artifact_id() {
                let artifact = update_blims_artifact_status("artifact.reject", artifact_id).await?;
                dashboard.update_artifact(artifact);
                dashboard.status = "Artifact rejected.".to_string();
            } else {
                dashboard.status = "Select a proposal or artifact to reject.".to_string();
            }
        }
        KeyCode::Char('f') => {
            if let Some(proposal_id) = dashboard.selected_proposal_id() {
                let proposal = update_blims_proposal_status("proposal.defer", proposal_id).await?;
                dashboard.update_proposal(proposal);
                dashboard.status = "Proposal deferred.".to_string();
            } else if let Some(artifact_id) = dashboard.selected_artifact_id() {
                let artifact = update_blims_artifact_status("artifact.defer", artifact_id).await?;
                dashboard.update_artifact(artifact);
                dashboard.status = "Artifact deferred.".to_string();
            } else {
                dashboard.status = "Select a proposal or artifact to defer.".to_string();
            }
        }
        _ => {}
    }
    Ok(true)
}

fn handle_blims_tui_picker_key(app: &mut BlimsTuiApp, key: KeyCode) -> bool {
    if !app.show_world_picker {
        return false;
    }
    match key {
        KeyCode::Escape => app.show_world_picker = false,
        KeyCode::Up => app.select_previous_template(),
        KeyCode::Down => app.select_next_template(),
        KeyCode::Enter => app.activate_selected_template(),
        _ => return false,
    }
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsConversationHandle {
    agent_id: String,
    conversation_ref: String,
    session: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsConversationLine {
    speaker: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsConversationState {
    handle: BlimsConversationHandle,
    agent_name: String,
    input: String,
    transcript: Vec<BlimsConversationLine>,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BlimsTuiMode {
    Office,
    Conversation(BlimsConversationState),
    Dashboard(BlimsCeoDashboardState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsWorldTickResult {
    world: BlimsWorldSnapshot,
    activity: Vec<BlimsActivityItem>,
    inbox: Vec<BlimsCeoInboxItem>,
}

#[derive(Debug)]
enum BlimsBackgroundJob {
    WorldTick(Receiver<Result<BlimsWorldTickResult, CliError>>),
    PlayerRoomSync(Receiver<Result<BlimsWorldSnapshot, CliError>>),
    WorldTemplateSelect(Receiver<Result<BlimsWorldSelection, CliError>>),
    DashboardLoad(Receiver<Result<BlimsCeoDashboardState, CliError>>),
}

#[derive(Debug)]
struct BlimsBackgroundJobs {
    world_tick: Option<BlimsBackgroundJob>,
    player_room_sync: Option<BlimsBackgroundJob>,
    world_template_select: Option<BlimsBackgroundJob>,
    dashboard_load: Option<BlimsBackgroundJob>,
}

impl BlimsBackgroundJobs {
    const fn new() -> Self {
        Self {
            world_tick: None,
            player_room_sync: None,
            world_template_select: None,
            dashboard_load: None,
        }
    }
}

#[derive(Debug)]
struct BlimsTuiApp {
    world: BlimsWorldSnapshot,
    report: BlimsMorningReport,
    interactions: BlimsAvailableInteractions,
    geometry: BlimsWorldGeometry,
    templates: Vec<BlimsWorldTemplateSummary>,
    agent_sprites: BTreeMap<String, BlimsAgentSpriteState>,
    live_log: Vec<String>,
    activity: Vec<BlimsActivityItem>,
    inbox: Vec<BlimsCeoInboxItem>,
    selected_inbox_item: usize,
    selected_template: usize,
    player_tile: BlimsTilePosition,
    selected_interaction: usize,
    pending_player_room_sync: Option<String>,
    pending_world_template_name: Option<String>,
    last_world_tick: Instant,
    last_animation: Instant,
    tick_count: u64,
    jobs: BlimsBackgroundJobs,
    mode: BlimsTuiMode,
    show_world_picker: bool,
    show_help: bool,
    status: String,
}

impl BlimsTuiApp {
    async fn load() -> Result<Self, CliError> {
        let world = load_blims_world().await?;
        let report = load_blims_report().await?;
        let interactions = load_blims_interactions().await?;
        let templates = load_world_templates().await?;
        let selected_template = templates
            .iter()
            .position(|template| template.id == world.template_id)
            .unwrap_or_default();
        let player_tile = player_tile_for_room(&world, &interactions.room_id);
        let geometry = BlimsWorldGeometry::from_world(&world);
        let agent_sprites = initial_agent_sprites(&world, &geometry);
        Ok(Self {
            world,
            report,
            interactions,
            geometry,
            templates,
            agent_sprites,
            live_log: vec!["Blims office is live.".to_string()],
            activity: load_blims_activity().await.unwrap_or_default(),
            inbox: load_blims_inbox().await.unwrap_or_default(),
            selected_inbox_item: 0,
            selected_template,
            player_tile,
            selected_interaction: 0,
            pending_player_room_sync: None,
            pending_world_template_name: None,
            last_world_tick: Instant::now(),
            last_animation: Instant::now(),
            tick_count: 0,
            jobs: BlimsBackgroundJobs::new(),
            mode: BlimsTuiMode::Office,
            show_world_picker: false,
            show_help: false,
            status: "Welcome CEO — choose a world with w, move with arrows/h/l.".to_string(),
        })
    }

    fn move_player_by(&mut self, dx: i64, dy: i64) {
        let next = BlimsTilePosition {
            x: self.player_tile.x + dx,
            y: self.player_tile.y + dy,
        };
        if !self.geometry.is_walkable(next) {
            self.status = "Bump — office wall.".to_string();
            return;
        }
        self.player_tile = next;
        if let Some(room_id) = self.geometry.room_id_at(next)
            && room_id != self.interactions.room_id
        {
            self.interactions.room_id.clone_from(&room_id);
            self.pending_player_room_sync = Some(room_id);
            self.clamp_selected_interaction();
            self.status = format!("Entered {}", self.current_room_name());
        }
    }

    async fn activate_primary_action(&mut self) -> Result<(), CliError> {
        if self.activate_selected_inbox_item().await? {
            return Ok(());
        }
        self.activate_selected_interaction().await
    }

    async fn activate_selected_inbox_item(&mut self) -> Result<bool, CliError> {
        let Some(item) = self.inbox.get(self.selected_inbox_item).cloned() else {
            return Ok(false);
        };
        if let Some(work_id) = item.action_command.strip_prefix("ai_work.start:") {
            self.status = format!("Starting AI work {work_id}…");
            start_blims_prepared_ai_work_detached(work_id.to_string()).await?;
            self.status = format!("Started AI work: {}", item.title);
            self.inbox = load_blims_inbox().await.unwrap_or_default();
            self.activity = load_blims_activity().await.unwrap_or_default();
            self.clamp_selected_inbox_item();
            return Ok(true);
        }
        if item.action_command.starts_with("proposal.review:")
            || item.action_command.starts_with("artifact.review:")
        {
            self.open_ceo_dashboard();
            return Ok(true);
        }
        if !item.actor_id.is_empty() {
            self.open_agent_conversation(item.actor_id).await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn activate_selected_interaction(&mut self) -> Result<(), CliError> {
        let Some(interaction) = self
            .nearby_interactions()
            .get(self.selected_interaction)
            .cloned()
        else {
            self.status = "Nothing nearby to use yet.".to_string();
            return Ok(());
        };
        if let Some(agent_id) = interaction.command.strip_prefix("ai ") {
            self.open_agent_conversation(agent_id.to_string()).await?;
        } else {
            self.status = format!("{} — `{}`", interaction.label, interaction.command);
        }
        Ok(())
    }

    async fn open_agent_conversation(&mut self, agent_id: String) -> Result<(), CliError> {
        self.freeze_agent_sprite(&agent_id);
        let handle = create_blims_agent_conversation(agent_id.clone()).await?;
        let agent_name = self
            .world
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .map_or_else(|| agent_id.clone(), |agent| agent.name.clone());
        let mut conversation = BlimsConversationState {
            handle,
            agent_name,
            input: String::new(),
            transcript: Vec::new(),
            status: "Conversation opened.".to_string(),
        };
        refresh_conversation_transcript(&mut conversation).await?;
        self.mode = BlimsTuiMode::Conversation(conversation);
        Ok(())
    }

    fn freeze_agent_sprite(&mut self, agent_id: &str) {
        if let Some(sprite) = self.agent_sprites.get_mut(agent_id) {
            sprite.target = sprite.position;
            sprite.path.clear();
        }
    }

    async fn open_nearest_agent_conversation(&mut self) -> Result<(), CliError> {
        let Some(agent_id) = self.nearest_talkable_agent_id() else {
            self.status = "Walk next to a coworker, then press t to talk.".to_string();
            return Ok(());
        };
        self.open_agent_conversation(agent_id).await
    }

    fn nearest_talkable_agent_id(&self) -> Option<String> {
        self.world
            .agents
            .iter()
            .filter_map(|agent| {
                let sprite = self.agent_sprites.get(&agent.id)?;
                let distance = manhattan_distance(self.player_tile, sprite.position);
                (distance <= 2).then(|| (distance, agent.id.clone()))
            })
            .min_by_key(|(distance, agent_id)| (*distance, agent_id.clone()))
            .map(|(_, agent_id)| agent_id)
    }

    fn active_conversation_agent_id(&self) -> Option<String> {
        match &self.mode {
            BlimsTuiMode::Conversation(conversation) => Some(conversation.handle.agent_id.clone()),
            BlimsTuiMode::Office | BlimsTuiMode::Dashboard(_) => None,
        }
    }

    fn open_ceo_dashboard(&mut self) {
        self.mode = BlimsTuiMode::Dashboard(BlimsCeoDashboardState::loading(
            "Loading CEO dashboard…".to_string(),
        ));
        self.status = "Loading CEO dashboard…".to_string();
        if self.jobs.dashboard_load.is_none() {
            self.jobs.dashboard_load = Some(BlimsBackgroundJob::DashboardLoad(
                spawn_blims_background(|| Box::pin(load_ceo_dashboard())),
            ));
        }
    }

    fn select_next_interaction(&mut self) {
        let count = self.nearby_interactions().len();
        if count > 0 {
            self.selected_interaction = (self.selected_interaction + 1) % count;
        }
    }

    fn select_next_inbox_item(&mut self) {
        if !self.inbox.is_empty() {
            self.selected_inbox_item = (self.selected_inbox_item + 1) % self.inbox.len();
            self.status = format!("Inbox: {}", self.inbox[self.selected_inbox_item].title);
        }
    }

    fn select_previous_inbox_item(&mut self) {
        if !self.inbox.is_empty() {
            self.selected_inbox_item = self
                .selected_inbox_item
                .checked_sub(1)
                .unwrap_or_else(|| self.inbox.len().saturating_sub(1));
            self.status = format!("Inbox: {}", self.inbox[self.selected_inbox_item].title);
        }
    }

    const fn clamp_selected_inbox_item(&mut self) {
        if self.inbox.is_empty() {
            self.selected_inbox_item = 0;
        } else if self.selected_inbox_item >= self.inbox.len() {
            self.selected_inbox_item = self.inbox.len() - 1;
        }
    }

    fn clamp_selected_interaction(&mut self) {
        let count = self.nearby_interactions().len();
        if count == 0 {
            self.selected_interaction = 0;
        } else if self.selected_interaction >= count {
            self.selected_interaction = count - 1;
        }
    }

    fn nearby_interactions(&self) -> Vec<BlimsWorldInteraction> {
        let mut interactions =
            if is_near_current_room(&self.world, self.player_tile, &self.interactions.room_id) {
                self.interactions.interactions.clone()
            } else {
                Vec::new()
            };
        interactions.extend(self.world.agents.iter().filter_map(|agent| {
            let sprite = self.agent_sprites.get(&agent.id)?;
            (manhattan_distance(self.player_tile, sprite.position) <= 2).then(|| {
                BlimsWorldInteraction {
                    id: format!("talk-{}", agent.id),
                    label: format!("Talk to {}", agent.name),
                    command: format!("ai {}", agent.id),
                    source: "agent".to_string(),
                }
            })
        }));
        interactions
    }

    fn should_world_tick(&self) -> bool {
        matches!(self.mode, BlimsTuiMode::Office)
            && self.jobs.world_template_select.is_none()
            && self.jobs.dashboard_load.is_none()
            && self.active_conversation_agent_id().is_none()
            && self.last_world_tick.elapsed() >= Duration::from_millis(900)
    }

    fn schedule_background_jobs(&mut self) {
        if self.should_world_tick()
            && self.jobs.world_tick.is_none()
            && self.jobs.dashboard_load.is_none()
            && self.pending_world_template_name.is_none()
        {
            self.tick_count = self.tick_count.saturating_add(1);
            self.last_world_tick = Instant::now();
            let tick_count = self.tick_count;
            self.jobs.world_tick = Some(BlimsBackgroundJob::WorldTick(spawn_blims_background(
                move || Box::pin(tick_blims_world_blocking(tick_count)),
            )));
        }
        if let Some(room_id) = self.pending_player_room_sync.take() {
            self.jobs.player_room_sync =
                Some(BlimsBackgroundJob::PlayerRoomSync(spawn_blims_background(
                    move || Box::pin(async move { move_blims_player_blocking(&room_id).await }),
                )));
        }
        if let Some(template_id) = self.pending_world_template_name.take() {
            self.jobs.world_template_select = Some(BlimsBackgroundJob::WorldTemplateSelect(
                spawn_blims_background(move || {
                    Box::pin(async move {
                        select_world_template_state(template_id)
                            .await
                            .map(|(selection, _)| selection)
                    })
                }),
            ));
        }
    }

    fn poll_background_jobs(&mut self) -> bool {
        let mut dirty = self.poll_world_tick_job();
        dirty |= self.poll_player_room_sync_job();
        dirty |= self.poll_world_template_select_job();
        dirty |= self.poll_dashboard_load_job();
        dirty
    }

    fn poll_world_tick_job(&mut self) -> bool {
        let mut dirty = false;
        let active_conversation_agent_id = self.active_conversation_agent_id();
        if let Some(BlimsBackgroundJob::WorldTick(receiver)) = &self.jobs.world_tick {
            match receiver.try_recv() {
                Ok(Ok(mut result)) => {
                    let world = &mut result.world;
                    if let Some(agent_id) = &active_conversation_agent_id {
                        keep_conversation_agent_in_place(
                            &self.world,
                            world,
                            &mut self.agent_sprites,
                            agent_id,
                        );
                    }
                    let previous = self.world.clone();
                    self.apply_world_snapshot(result.world);
                    self.activity = result.activity;
                    self.inbox = result.inbox;
                    self.record_world_changes(&previous);
                    self.jobs.world_tick = None;
                    dirty = true;
                }
                Ok(Err(error)) => {
                    self.status = format!("world tick failed: {error}");
                    self.jobs.world_tick = None;
                    dirty = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status = "world tick worker disconnected".to_string();
                    self.jobs.world_tick = None;
                    dirty = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        dirty
    }

    fn poll_player_room_sync_job(&mut self) -> bool {
        let mut dirty = false;
        if let Some(BlimsBackgroundJob::PlayerRoomSync(receiver)) = &self.jobs.player_room_sync {
            match receiver.try_recv() {
                Ok(Ok(world)) => {
                    self.apply_world_snapshot(world);
                    self.jobs.player_room_sync = None;
                    dirty = true;
                }
                Ok(Err(error)) => {
                    self.status = format!("room sync failed: {error}");
                    self.jobs.player_room_sync = None;
                    dirty = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status = "room sync worker disconnected".to_string();
                    self.jobs.player_room_sync = None;
                    dirty = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        dirty
    }

    fn poll_world_template_select_job(&mut self) -> bool {
        let mut dirty = false;
        if let Some(BlimsBackgroundJob::WorldTemplateSelect(receiver)) =
            &self.jobs.world_template_select
        {
            match receiver.try_recv() {
                Ok(Ok(selection)) => {
                    self.apply_world_snapshot(selection.world);
                    self.report = selection.report;
                    self.interactions = selection.interactions;
                    self.player_tile =
                        player_tile_for_room(&self.world, &self.interactions.room_id);
                    self.clamp_selected_interaction();
                    self.show_world_picker = false;
                    self.status = format!("Selected {}", self.world.theme);
                    self.jobs.world_template_select = None;
                    self.last_world_tick = Instant::now();
                    dirty = true;
                }
                Ok(Err(error)) => {
                    self.status = format!("world select failed: {error}");
                    self.jobs.world_template_select = None;
                    dirty = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status = "world select worker disconnected".to_string();
                    self.jobs.world_template_select = None;
                    dirty = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        dirty
    }

    fn poll_dashboard_load_job(&mut self) -> bool {
        let mut dirty = false;
        if let Some(BlimsBackgroundJob::DashboardLoad(receiver)) = &self.jobs.dashboard_load {
            if !matches!(self.mode, BlimsTuiMode::Dashboard(_)) {
                self.jobs.dashboard_load = None;
                return true;
            }
            match receiver.try_recv() {
                Ok(Ok(dashboard)) => {
                    if matches!(self.mode, BlimsTuiMode::Dashboard(_)) {
                        self.mode = BlimsTuiMode::Dashboard(dashboard);
                        self.status = "CEO dashboard loaded.".to_string();
                    }
                    self.jobs.dashboard_load = None;
                    dirty = true;
                }
                Ok(Err(error)) => {
                    if let BlimsTuiMode::Dashboard(dashboard) = &mut self.mode {
                        dashboard.status = format!("dashboard load failed: {error}");
                        self.status = format!("dashboard load failed: {error}");
                    }
                    self.jobs.dashboard_load = None;
                    dirty = true;
                }
                Err(TryRecvError::Disconnected) => {
                    if let BlimsTuiMode::Dashboard(dashboard) = &mut self.mode {
                        dashboard.status = "dashboard loader disconnected".to_string();
                        self.status = "dashboard loader disconnected".to_string();
                    }
                    self.jobs.dashboard_load = None;
                    dirty = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        dirty
    }

    fn apply_world_snapshot(&mut self, world: BlimsWorldSnapshot) {
        self.world = world;
        self.geometry = BlimsWorldGeometry::from_world(&self.world);
        self.sync_agent_targets();
        self.clamp_selected_interaction();
    }

    fn animate_agents(&mut self) -> bool {
        if self.last_animation.elapsed() < Duration::from_millis(160) {
            return false;
        }
        let active_conversation_agent_id = self.active_conversation_agent_id();
        let mut dirty = false;
        let targets = self
            .world
            .agents
            .iter()
            .map(|agent| {
                (
                    agent.id.clone(),
                    agent_tile(&self.world, &self.geometry, agent),
                )
            })
            .collect::<Vec<_>>();
        for (agent_id, target) in targets {
            let sprite = self
                .agent_sprites
                .entry(agent_id.clone())
                .or_insert_with(|| BlimsAgentSpriteState::new(target));
            if active_conversation_agent_id.as_ref() == Some(&agent_id) {
                sprite.target = sprite.position;
                sprite.path.clear();
                continue;
            }
            if sprite.target != target {
                sprite.target = target;
                sprite.path.clear();
            }
            let next = self.geometry.next_walkable_step(sprite);
            if next != sprite.position {
                dirty = true;
                sprite.position = next;
            }
        }
        self.last_animation = Instant::now();
        dirty
    }

    fn sync_agent_targets(&mut self) {
        let active_conversation_agent_id = self.active_conversation_agent_id();
        for agent in &self.world.agents {
            let target = agent_tile(&self.world, &self.geometry, agent);
            let sprite = self
                .agent_sprites
                .entry(agent.id.clone())
                .or_insert_with(|| BlimsAgentSpriteState::new(target));
            if active_conversation_agent_id.as_ref() == Some(&agent.id) {
                sprite.target = sprite.position;
                sprite.path.clear();
                continue;
            }
            if sprite.target != target {
                sprite.target = target;
                sprite.path.clear();
            }
        }
        self.agent_sprites
            .retain(|agent_id, _| self.world.agents.iter().any(|agent| agent.id == *agent_id));
    }

    fn record_world_changes(&mut self, previous: &BlimsWorldSnapshot) {
        let mut messages = Vec::new();
        for agent in &self.world.agents {
            if let Some(before) = previous.agents.iter().find(|before| before.id == agent.id) {
                if before.room_id != agent.room_id {
                    messages.push(format!("{} walked to {}.", agent.name, agent.room_id));
                }
                if before.status != agent.status {
                    messages.push(format!("{}: {}", agent.name, agent.status));
                }
            }
        }
        for message in messages {
            self.push_live_log(message);
        }
    }

    fn push_live_log(&mut self, message: String) {
        self.live_log.insert(0, message);
        self.live_log.truncate(6);
    }

    async fn move_next(&mut self) -> Result<(), CliError> {
        let next = next_room_id(&self.world, &self.interactions.room_id);
        self.world = move_blims_player_blocking(&next).await?;
        self.interactions = load_blims_interactions().await?;
        self.player_tile = player_tile_for_room(&self.world, &self.interactions.room_id);
        self.clamp_selected_interaction();
        self.status = format!("Moved to {}", self.current_room_name());
        Ok(())
    }

    async fn move_previous(&mut self) -> Result<(), CliError> {
        let previous = previous_room_id(&self.world, &self.interactions.room_id);
        self.world = move_blims_player_blocking(&previous).await?;
        self.interactions = load_blims_interactions().await?;
        self.player_tile = player_tile_for_room(&self.world, &self.interactions.room_id);
        self.clamp_selected_interaction();
        self.status = format!("Moved to {}", self.current_room_name());
        Ok(())
    }

    fn activate_selected_template(&mut self) {
        if !self.show_world_picker || self.jobs.world_template_select.is_some() {
            return;
        }
        if let Some(template) = self.templates.get(self.selected_template) {
            self.pending_world_template_name = Some(template.id.clone());
            self.status = format!("Switching office to {}…", template.name);
        }
    }

    const fn select_next_template(&mut self) {
        if self.templates.is_empty() {
            return;
        }
        self.selected_template = (self.selected_template + 1) % self.templates.len();
    }

    fn select_previous_template(&mut self) {
        if self.templates.is_empty() {
            return;
        }
        self.selected_template = self
            .selected_template
            .checked_sub(1)
            .unwrap_or_else(|| self.templates.len().saturating_sub(1));
    }

    fn current_room_name(&self) -> String {
        self.world
            .rooms
            .iter()
            .find(|room| room.id == self.interactions.room_id)
            .map_or_else(|| "the hallway".to_string(), |room| room.name.clone())
    }

    fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.fill(area, " ", Style::new().bg(Color::Rgb(12, 10, 18)));
        let rows = split(
            area,
            Direction::Vertical,
            &[
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ],
        );
        if rows.len() < 3 {
            return;
        }
        render_blims_header(self, rows[0], frame);
        let cols = split(
            rows[1],
            Direction::Horizontal,
            &[Constraint::Min(54), Constraint::Length(42)],
        );
        if cols.len() == 2 {
            render_blims_map(self, cols[0], frame);
            render_blims_sidebar(self, cols[1], frame);
        } else {
            render_blims_map(self, rows[1], frame);
        }
        render_blims_footer(self, rows[2], frame);
        if self.show_world_picker {
            render_world_picker(self, area, frame);
        }
        if self.show_help {
            render_blims_help_modal(area, frame);
        }
        if let BlimsTuiMode::Conversation(conversation) = &self.mode {
            render_conversation_modal(conversation, area, frame);
        }
        if let BlimsTuiMode::Dashboard(dashboard) = &self.mode {
            render_ceo_dashboard_modal(dashboard, area, frame);
        }
    }
}

async fn load_ceo_dashboard() -> Result<BlimsCeoDashboardState, CliError> {
    let operation =
        submit_blims_command(&serde_json::json!({ "type": "refresh_dashboard" })).await?;
    let projection = load_ceo_dashboard_projection().await?;
    Ok(BlimsCeoDashboardState {
        initiatives: projection.initiatives,
        tasks: projection.tasks,
        proposals: projection.proposals,
        artifacts: projection.artifacts,
        guidance: projection.guidance,
        selected_section: 0,
        selected_item: 0,
        status: format!("CEO dashboard loaded via {}.", operation.operation_id),
    })
}

async fn load_ceo_dashboard_projection() -> Result<BlimsDashboardProjection, CliError> {
    let response =
        call_blims_service("projection.dashboard.get", blims_workspace_payload()?).await?;
    decode_blims_response::<BlimsDashboardProjection>(response)
}

async fn submit_blims_command(
    command: &serde_json::Value,
) -> Result<BlimsCommandSubmitResponse, CliError> {
    let request = blims_command_request(command)?;
    let response = call_blims_service("command.submit", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<BlimsCommandSubmitResponse>(response)
}

fn blims_command_request(command: &serde_json::Value) -> Result<serde_json::Value, CliError> {
    let command_type = command
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("command");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |duration| duration.as_millis());
    Ok(serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "command_id": format!("tui-{command_type}-{now_ms}"),
        "actor": "ceo",
        "frontend_id": "tui",
        "command": command,
    }))
}

async fn load_blims_world() -> Result<BlimsWorldSnapshot, CliError> {
    let response = call_blims_service("projection.world.get", blims_workspace_payload()?).await?;
    decode_blims_response::<BlimsWorldSnapshot>(response)
}

async fn load_blims_interactions() -> Result<BlimsAvailableInteractions, CliError> {
    let response =
        call_blims_service("world.available_interactions", blims_workspace_payload()?).await?;
    decode_blims_response::<BlimsAvailableInteractions>(response)
}

async fn load_blims_activity() -> Result<Vec<BlimsActivityItem>, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "limit": 12_u64,
    });
    let response =
        call_blims_service("projection.activity.get", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<Vec<BlimsActivityItem>>(response)
}

async fn load_blims_inbox() -> Result<Vec<BlimsCeoInboxItem>, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "limit": 12_u64,
    });
    let response =
        call_blims_service("projection.ceo_inbox.get", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<Vec<BlimsCeoInboxItem>>(response)
}

async fn move_blims_player_blocking(room_id: &str) -> Result<BlimsWorldSnapshot, CliError> {
    let _operation = submit_blims_command(&serde_json::json!({
        "type": "move_player",
        "room_id": room_id,
    }))
    .await?;
    load_blims_world().await
}

async fn tick_blims_world_blocking(tick_count: u64) -> Result<BlimsWorldTickResult, CliError> {
    let mut world = load_blims_world().await?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_i64, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        });
    let _operation = submit_blims_command(&serde_json::json!({
        "type": "tick_world",
        "tick_id": format!("tui-{tick_count}"),
        "now_ms": now_ms,
    }))
    .await?;
    let tick_world = load_blims_world().await?;
    if tick_world != world {
        return Ok(BlimsWorldTickResult {
            world: tick_world,
            activity: load_blims_activity().await.unwrap_or_default(),
            inbox: load_blims_inbox().await.unwrap_or_default(),
        });
    }
    let Some((agent_id, room_id)) = next_visible_agent_move(&world, tick_count) else {
        return Ok(BlimsWorldTickResult {
            world,
            activity: load_blims_activity().await.unwrap_or_default(),
            inbox: load_blims_inbox().await.unwrap_or_default(),
        });
    };
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": agent_id,
        "room_id": room_id,
        "correlation_id": format!("tui-visible-agent-move-{tick_count}"),
        "causation_id": format!("tui-{tick_count}"),
    });
    let response = call_blims_service("agent.move", serde_json::to_vec(&request)?).await?;
    let moved = decode_blims_response::<BlimsAgentSnapshot>(response)?;
    if let Some(agent) = world.agents.iter_mut().find(|agent| agent.id == moved.id) {
        *agent = moved;
    }
    Ok(BlimsWorldTickResult {
        world,
        activity: load_blims_activity().await.unwrap_or_default(),
        inbox: load_blims_inbox().await.unwrap_or_default(),
    })
}

fn next_visible_agent_move(
    world: &BlimsWorldSnapshot,
    tick_count: u64,
) -> Option<(String, String)> {
    if world.agents.is_empty() || world.rooms.len() < 2 {
        return None;
    }
    let agent_index = usize::try_from(tick_count).ok()? % world.agents.len();
    let agent = world.agents.get(agent_index)?;
    let candidate_rooms = world
        .rooms
        .iter()
        .filter(|room| room.id != agent.room_id)
        .collect::<Vec<_>>();
    if candidate_rooms.is_empty() {
        return None;
    }
    let room_index = usize::try_from(tick_count / 2).ok()? % candidate_rooms.len();
    Some((
        agent.id.clone(),
        candidate_rooms.get(room_index)?.id.clone(),
    ))
}

async fn load_blims_report() -> Result<BlimsMorningReport, CliError> {
    let response = call_blims_service("report.morning", blims_workspace_payload()?).await?;
    decode_blims_response::<BlimsMorningReport>(response)
}

async fn load_world_templates() -> Result<Vec<BlimsWorldTemplateSummary>, CliError> {
    let response = call_blims_service("world.template_list", blims_workspace_payload()?).await?;
    decode_blims_response::<Vec<BlimsWorldTemplateSummary>>(response)
}

fn spawn_blims_background<T>(
    operation: impl FnOnce() -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, CliError>> + Send>,
    > + Send
    + 'static,
) -> Receiver<Result<T, CliError>>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    tokio::spawn(async move {
        let result = operation().await;
        let _ = sender.send(result);
    });
    receiver
}

async fn refresh_conversation_transcript(
    conversation: &mut BlimsConversationState,
) -> Result<(), CliError> {
    let events = BcodeClient::default_endpoint()
        .session_history(conversation.handle.session)
        .await?;
    conversation.transcript = events
        .into_iter()
        .filter_map(conversation_line_from_session_event)
        .collect();
    Ok(())
}

fn conversation_line_from_session_event(
    event: bcode_session_models::SessionEvent,
) -> Option<BlimsConversationLine> {
    match event.kind {
        bcode_session_models::SessionEventKind::UserMessage { text, .. } => {
            Some(BlimsConversationLine {
                speaker: "CEO".to_string(),
                text,
            })
        }
        bcode_session_models::SessionEventKind::AssistantMessage { text }
        | bcode_session_models::SessionEventKind::AssistantDelta { text } => {
            Some(BlimsConversationLine {
                speaker: "Agent".to_string(),
                text,
            })
        }
        bcode_session_models::SessionEventKind::ToolCallRequested { tool_name, .. } => {
            Some(BlimsConversationLine {
                speaker: "Tool".to_string(),
                text: format!("requested {tool_name}"),
            })
        }
        bcode_session_models::SessionEventKind::ToolCallFinished {
            result, is_error, ..
        } => Some(BlimsConversationLine {
            speaker: if is_error { "Tool error" } else { "Tool" }.to_string(),
            text: result,
        }),
        _ => None,
    }
}

fn render_blims_header(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightMagenta)))
        .background(Style::new().bg(Color::Rgb(19, 16, 30)))
        .render(area, frame);
    let title = Line::from_spans(vec![
        Span::styled(
            " ✨ BLIMS ",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            app.world.theme.clone(),
            Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  [{}]", app.world.template_id),
            Style::new().fg(Color::BrightBlack),
        ),
    ]);
    frame.write_line_with_fallback_style(
        area.inset(Insets::new(1, 2, 0, 2)),
        &title,
        Style::new().bg(Color::Rgb(19, 16, 30)),
    );
}

fn render_blims_footer(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightBlue)))
        .background(Style::new().bg(Color::Rgb(10, 14, 24)))
        .render(area, frame);
    let line = Line::from_spans(vec![
        Span::styled(
            " arrows/hjkl ",
            Style::new()
                .fg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("walk  "),
        Span::styled(
            "e/enter",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" act  "),
        Span::styled(
            "n/N",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" inbox  "),
        Span::styled(
            "t",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" talk  "),
        Span::styled(
            "esc",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" back  "),
        Span::styled(
            "w",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" worlds  "),
        Span::styled(
            "r/?/q",
            Style::new()
                .fg(Color::BrightRed)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" report/help/quit — "),
        Span::styled(app.status.clone(), Style::new().fg(Color::BrightGreen)),
    ]);
    frame.write_line_with_fallback_style(
        area.inset(Insets::new(1, 2, 0, 2)),
        &line,
        Style::new().bg(Color::Rgb(10, 14, 24)),
    );
}

fn render_blims_map(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightCyan)))
        .title(" Pixel office ")
        .background(Style::new().bg(Color::Rgb(8, 18, 24)));
    panel.render(area, frame);
    let inner = panel.inner_area(area).inset(Insets::all(1));
    render_pixel_world(app, inner, frame);
}

fn render_pixel_world(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let viewport = BlimsViewport::for_area(&app.geometry, app.player_tile, area);
    for screen_y in 0..area.height {
        for screen_x in 0..area.width {
            let tile = BlimsTilePosition {
                x: viewport.origin.x + i64::from(screen_x),
                y: viewport.origin.y + i64::from(screen_y),
            };
            let (glyph, style) = tile_glyph(app, tile);
            frame.buffer_mut().set_cell(
                Point::new(area.x + screen_x, area.y + screen_y),
                glyph,
                style,
            );
        }
    }
    render_agent_thought_bubbles(app, area, &viewport, frame);
}

fn render_agent_thought_bubbles(
    app: &BlimsTuiApp,
    area: Rect,
    viewport: &BlimsViewport,
    frame: &mut Frame<'_>,
) {
    for agent in &app.world.agents {
        let Some(sprite) = app.agent_sprites.get(&agent.id) else {
            continue;
        };
        let Some(message) = agent_activity_bubble(app, &agent.id) else {
            continue;
        };
        let screen_x = sprite.position.x - viewport.origin.x + 1;
        let screen_y = sprite.position.y - viewport.origin.y - 1;
        if screen_x < 0 || screen_y < 0 {
            continue;
        }
        let Ok(screen_x) = u16::try_from(screen_x) else {
            continue;
        };
        let Ok(screen_y) = u16::try_from(screen_y) else {
            continue;
        };
        if screen_x >= area.width || screen_y >= area.height {
            continue;
        }
        let text = format!("“{}”", truncate_chars(&message, 20));
        frame.write_line_with_fallback_style(
            Rect::new(
                area.x + screen_x,
                area.y + screen_y,
                area.width - screen_x,
                1,
            ),
            &Line::raw(text),
            Style::new()
                .fg(Color::BrightYellow)
                .bg(Color::Rgb(8, 18, 24)),
        );
    }
}

fn agent_activity_bubble(app: &BlimsTuiApp, agent_id: &str) -> Option<String> {
    app.inbox
        .iter()
        .find(|item| item.actor_id == agent_id)
        .map(|item| item.action_label.clone())
        .or_else(|| {
            app.activity
                .iter()
                .find(|item| item.actor_id == agent_id && !item.action_hint.is_empty())
                .map(|item| item.action_hint.clone())
        })
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

fn tile_glyph(app: &BlimsTuiApp, tile: BlimsTilePosition) -> (&'static str, Style) {
    if tile == app.player_tile {
        return (
            "@",
            Style::new()
                .fg(Color::BrightYellow)
                .bg(Color::Rgb(58, 40, 78))
                .add_modifier(Modifier::BOLD),
        );
    }
    if let Some(agent) = app.world.agents.iter().find(|agent| {
        app.agent_sprites
            .get(&agent.id)
            .is_some_and(|sprite| sprite.position == tile)
    }) {
        return (
            agent_sprite(agent),
            Style::new()
                .fg(Color::BrightGreen)
                .bg(Color::Rgb(24, 36, 34))
                .add_modifier(Modifier::BOLD),
        );
    }
    let Some(room) = app.geometry.room_at(&app.world, tile) else {
        return (
            " ",
            Style::new()
                .fg(Color::BrightBlack)
                .bg(Color::Rgb(5, 10, 14)),
        );
    };
    let rect = room_tile_rect(room);
    let bg = room_bg(room);
    let fg = color_from_name(&room.color);
    if tile.x == rect.x || tile.y == rect.y || tile.x == rect.right() || tile.y == rect.bottom() {
        return ("▓", Style::new().fg(fg).bg(Color::Rgb(9, 12, 20)));
    }
    if tile == room_anchor(room) {
        return (
            room_symbol(room),
            Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        );
    }
    if app.geometry.corridors.contains(&tile) {
        return (
            "░",
            Style::new()
                .fg(Color::BrightBlack)
                .bg(Color::Rgb(18, 18, 24)),
        );
    }
    ("·", Style::new().fg(Color::Rgb(88, 92, 104)).bg(bg))
}

fn agent_sprite(agent: &BlimsAgentSnapshot) -> &'static str {
    match agent.role.as_str() {
        role if role.contains("Engineer") || role.contains("developer") => "E",
        role if role.contains("Designer") || role.contains("Creative") => "D",
        role if role.contains("Reviewer") || role.contains("QA") => "R",
        _ => "B",
    }
}

fn room_symbol(room: &BlimsRoomSnapshot) -> &'static str {
    match room.symbol.as_str() {
        "🏢" | "⌂" => "⌂",
        "🧠" => "*",
        "⚙" | "⚙️" => "⚙",
        "🎨" => "✦",
        "✓" | "✅" => "✓",
        "☕" => "☕",
        _ => "■",
    }
}

fn render_blims_sidebar(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightGreen)))
        .title(" CEO console ")
        .background(Style::new().bg(Color::Rgb(13, 20, 18)));
    panel.render(area, frame);
    let inner = panel.inner_area(area).inset(Insets::all(1));
    let rows = split(
        inner,
        Direction::Vertical,
        &[
            Constraint::Length(6),
            Constraint::Min(6),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(6),
        ],
    );
    if rows.len() != 5 {
        return;
    }
    render_current_room_panel(app, rows[0], frame);
    render_activity_panel(app, rows[1], frame);
    render_inbox_panel(app, rows[2], frame);
    render_interactions_panel(app, rows[3], frame);
    render_live_log_panel(app, rows[4], frame);
}

fn render_current_room_panel(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let room = app
        .world
        .rooms
        .iter()
        .find(|room| room.id == app.interactions.room_id);
    let text = room.map_or_else(
        || Text::raw("Between rooms"),
        |room| {
            Text::from_lines(vec![
                Line::from_spans(vec![Span::styled(
                    room.name.clone(),
                    Style::new()
                        .fg(Color::BrightYellow)
                        .add_modifier(Modifier::BOLD),
                )]),
                Line::raw(room.purpose.clone()),
                Line::raw(format!(
                    "{} · productivity +{}",
                    room.room_kind, room.productivity_modifier
                )),
                Line::raw(format!(
                    "CEO tile {},{} · room anchor {},{}",
                    app.player_tile.x,
                    app.player_tile.y,
                    room_anchor(room).x,
                    room_anchor(room).y
                )),
            ])
        },
    );
    TextBlock::new(text)
        .wrap(TextWrap::Character)
        .style(
            Style::new()
                .bg(Color::Rgb(13, 20, 18))
                .fg(Color::BrightWhite),
        )
        .render(area, frame);
}

fn render_activity_panel(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "🏢 Company pulse",
        Style::new()
            .fg(Color::BrightCyan)
            .add_modifier(Modifier::BOLD),
    )])];
    if app.activity.is_empty() {
        lines.push(Line::raw("No visible company activity yet."));
    } else {
        lines.extend(app.activity.iter().take(6).map(|item| {
            let marker = if item.severity == "attention" {
                "!"
            } else {
                "•"
            };
            let actor = if item.actor_id.is_empty() {
                String::new()
            } else {
                format!("{}: ", item.actor_id)
            };
            Line::raw(format!("{marker} {actor}{}", item.title))
        }));
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(13, 20, 18)).fg(Color::White))
        .render(area, frame);
}

fn render_inbox_panel(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "📬 CEO inbox",
        Style::new()
            .fg(Color::BrightYellow)
            .add_modifier(Modifier::BOLD),
    )])];
    if app.inbox.is_empty() {
        lines.push(Line::raw("Nothing needs CEO attention."));
    } else {
        lines.extend(app.inbox.iter().take(5).enumerate().map(|(index, item)| {
            let actor = if item.actor_id.is_empty() {
                String::new()
            } else {
                format!("{} · ", item.actor_id)
            };
            let prefix = if index == app.selected_inbox_item {
                "▶ "
            } else {
                "  "
            };
            Line::raw(format!("{prefix}{}{}", actor, item.title))
        }));
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(13, 20, 18)).fg(Color::White))
        .render(area, frame);
}

fn render_interactions_panel(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "Interactions",
        Style::new()
            .fg(Color::BrightMagenta)
            .add_modifier(Modifier::BOLD),
    )])];
    let nearby = app.nearby_interactions();
    if nearby.is_empty() {
        lines.push(Line::raw("Walk into a room or up to an agent."));
    } else {
        lines.extend(
            nearby
                .iter()
                .take(5)
                .enumerate()
                .map(|(index, interaction)| {
                    let selected = index == app.selected_interaction;
                    Line::from_spans(vec![
                        Span::styled(
                            if selected { "▶ " } else { "  " },
                            Style::new().fg(Color::BrightYellow),
                        ),
                        Span::styled(
                            format!("{} ", interaction.source),
                            Style::new().fg(Color::BrightBlack),
                        ),
                        Span::styled(
                            interaction.label.clone(),
                            if selected {
                                Style::new()
                                    .fg(Color::BrightYellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::new().fg(Color::White)
                            },
                        ),
                    ])
                }),
        );
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(13, 20, 18)).fg(Color::White))
        .render(area, frame);
}

fn render_live_log_panel(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "Live company",
        Style::new()
            .fg(Color::BrightGreen)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.extend(
        app.live_log
            .iter()
            .take(5)
            .map(|message| Line::raw(format!("• {message}"))),
    );
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(13, 20, 18)).fg(Color::White))
        .render(area, frame);
}

fn render_world_picker(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let picker = centered(area, Size::new(76, 18));
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightYellow)))
        .title(" Choose your Blims office ")
        .background(Style::new().bg(Color::Rgb(32, 24, 42)));
    panel.render(picker, frame);
    let inner = panel.inner_area(picker).inset(Insets::all(1));
    for (index, template) in app.templates.iter().enumerate() {
        let Ok(row) = u16::try_from(index.saturating_mul(4)) else {
            break;
        };
        if row >= inner.height {
            break;
        }
        let selected = index == app.selected_template;
        let bg = if selected {
            Color::Rgb(70, 48, 82)
        } else {
            Color::Rgb(32, 24, 42)
        };
        let style = Style::new().bg(bg).fg(if selected {
            Color::BrightYellow
        } else {
            Color::BrightWhite
        });
        let y = inner.y + row;
        frame.fill(
            Rect::new(inner.x, y, inner.width, 3),
            " ",
            Style::new().bg(bg),
        );
        frame.write_line_with_fallback_style(
            Rect::new(inner.x, y, inner.width, 1),
            &Line::from_spans(vec![
                Span::styled(
                    if selected { "▶ " } else { "  " },
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(template.name.clone(), style.add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("  {} rooms", template.rooms),
                    Style::new().bg(bg).fg(Color::BrightBlack),
                ),
            ]),
            Style::new().bg(bg),
        );
        frame.write_line_with_fallback_style(
            Rect::new(inner.x + 2, y + 1, inner.width.saturating_sub(2), 1),
            &Line::raw(template.description.clone()),
            Style::new().bg(bg).fg(Color::White),
        );
        frame.write_line_with_fallback_style(
            Rect::new(inner.x + 2, y + 2, inner.width.saturating_sub(2), 1),
            &Line::from_spans(vec![Span::styled(
                template.flavor.clone(),
                Style::new().bg(bg).fg(Color::BrightCyan),
            )]),
            Style::new().bg(bg),
        );
    }
    let hint_y = picker.bottom().saturating_sub(2);
    frame.write_line_with_fallback_style(
        Rect::new(inner.x, hint_y, inner.width, 1),
        &Line::raw("Enter selects · arrows move · q exits"),
        Style::new()
            .bg(Color::Rgb(32, 24, 42))
            .fg(Color::BrightBlack),
    );
}

fn render_ceo_dashboard_modal(
    dashboard: &BlimsCeoDashboardState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let modal = centered(area, Size::new(96, 30));
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightYellow)))
        .title(" CEO operating dashboard ")
        .background(Style::new().bg(Color::Rgb(16, 17, 28)));
    panel.render(modal, frame);
    let inner = panel.inner_area(modal).inset(Insets::all(1));
    let rows = split(
        inner,
        Direction::Vertical,
        &[
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Min(8),
            Constraint::Length(3),
        ],
    );
    if rows.len() != 4 {
        return;
    }
    TextBlock::new(Text::from_lines(vec![
        Line::from_spans(vec![Span::styled(
            "Company inbox",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::raw(format!(
            "{} initiative(s), {} task(s), {} proposal(s), {} artifact(s), {} active guidance item(s)",
            dashboard.initiatives.len(),
            dashboard.tasks.len(),
            dashboard.proposals.len(),
            dashboard.artifacts.len(),
            dashboard.guidance.len()
        )),
    ]))
    .style(
        Style::new()
            .bg(Color::Rgb(16, 17, 28))
            .fg(Color::BrightWhite),
    )
    .render(rows[0], frame);

    let columns = split(
        rows[1],
        Direction::Horizontal,
        &[Constraint::Percentage(50), Constraint::Percentage(50)],
    );
    if columns.len() == 2 {
        render_dashboard_agent_work(dashboard, columns[0], frame);
        render_dashboard_proposals(dashboard, columns[1], frame);
    }
    let bottom_columns = split(
        rows[2],
        Direction::Horizontal,
        &[Constraint::Percentage(50), Constraint::Percentage(50)],
    );
    if bottom_columns.len() == 2 {
        render_dashboard_artifacts(dashboard, bottom_columns[0], frame);
        render_dashboard_guidance(dashboard, bottom_columns[1], frame);
    }
    frame.write_line_with_fallback_style(
        rows[3],
        &Line::raw(format!(
            "tab/←/→ sections · ↑/↓ select · a approve · x reject · f defer · r refresh · d/Esc closes · {}",
            dashboard.status
        )),
        Style::new()
            .bg(Color::Rgb(16, 17, 28))
            .fg(Color::BrightBlack),
    );
}

fn render_dashboard_agent_work(
    dashboard: &BlimsCeoDashboardState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "Agent work",
        Style::new()
            .fg(Color::BrightCyan)
            .add_modifier(Modifier::BOLD),
    )])];
    if dashboard.tasks.is_empty() {
        lines.push(Line::raw("No tasks yet. Create or plan an initiative."));
    } else {
        lines.extend(
            dashboard
                .tasks
                .iter()
                .take(8)
                .enumerate()
                .map(|(index, task)| {
                    dashboard_line(
                        dashboard.selected_section == 0 && dashboard.selected_item == index,
                        format!(
                            "{} · {} · {} — {}",
                            task.assigned_agent_id, task.status, task.id, task.title
                        ),
                    )
                }),
        );
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(16, 17, 28)).fg(Color::White))
        .render(area, frame);
}

fn render_dashboard_proposals(
    dashboard: &BlimsCeoDashboardState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "Proposal inbox",
        Style::new()
            .fg(Color::BrightMagenta)
            .add_modifier(Modifier::BOLD),
    )])];
    if dashboard.proposals.is_empty() {
        lines.push(Line::raw("No proposals yet. Ask an agent to plan/work."));
    } else {
        lines.extend(
            dashboard
                .proposals
                .iter()
                .take(8)
                .enumerate()
                .map(|(index, proposal)| {
                    dashboard_line(
                        dashboard.selected_section == 1 && dashboard.selected_item == index,
                        format!(
                            "{} · {} · {} — {}",
                            proposal.status, proposal.agent_id, proposal.id, proposal.summary
                        ),
                    )
                }),
        );
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(16, 17, 28)).fg(Color::White))
        .render(area, frame);
}

fn render_dashboard_artifacts(
    dashboard: &BlimsCeoDashboardState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "Review artifacts",
        Style::new()
            .fg(Color::BrightGreen)
            .add_modifier(Modifier::BOLD),
    )])];
    if dashboard.artifacts.is_empty() {
        lines.push(Line::raw("No artifacts yet."));
    } else {
        lines.extend(
            dashboard
                .artifacts
                .iter()
                .take(8)
                .enumerate()
                .map(|(index, artifact)| {
                    dashboard_line(
                        dashboard.selected_section == 2 && dashboard.selected_item == index,
                        format!(
                            "{} · {} · {} — {}",
                            artifact.status, artifact.kind, artifact.id, artifact.title
                        ),
                    )
                }),
        );
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(16, 17, 28)).fg(Color::White))
        .render(area, frame);
}

fn render_dashboard_guidance(
    dashboard: &BlimsCeoDashboardState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        "CEO guidance / initiatives",
        Style::new()
            .fg(Color::BrightYellow)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.extend(
        dashboard
            .guidance
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, guidance)| {
                dashboard_line(
                    dashboard.selected_section == 3 && dashboard.selected_item == index,
                    format!("{} — {}", guidance.strength, guidance.guidance),
                )
            }),
    );
    let guidance_count = dashboard.guidance.len().min(3);
    lines.extend(
        dashboard
            .initiatives
            .iter()
            .take(5)
            .enumerate()
            .map(|(index, initiative)| {
                dashboard_line(
                    dashboard.selected_section == 3
                        && dashboard.selected_item == guidance_count.saturating_add(index),
                    format!(
                        "{} · p{} — {}",
                        initiative.status, initiative.priority, initiative.title
                    ),
                )
            }),
    );
    if lines.len() == 1 {
        lines.push(Line::raw("No guidance or initiatives yet."));
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(16, 17, 28)).fg(Color::White))
        .render(area, frame);
}

fn dashboard_line(selected: bool, text: String) -> Line {
    Line::from_spans(vec![
        Span::styled(
            if selected { "▶ " } else { "  " },
            Style::new().fg(Color::BrightYellow),
        ),
        Span::styled(
            text,
            if selected {
                Style::new()
                    .fg(Color::BrightYellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            },
        ),
    ])
}

fn render_conversation_modal(
    conversation: &BlimsConversationState,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let modal = centered(area, Size::new(86, 24));
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightMagenta)))
        .title(format!(" Talking with {} ", conversation.agent_name))
        .background(Style::new().bg(Color::Rgb(18, 14, 28)));
    panel.render(modal, frame);
    let inner = panel.inner_area(modal).inset(Insets::all(1));
    let rows = split(
        inner,
        Direction::Vertical,
        &[
            Constraint::Min(8),
            Constraint::Length(4),
            Constraint::Length(2),
        ],
    );
    if rows.len() != 3 {
        return;
    }
    let mut lines = Vec::new();
    for line in conversation.transcript.iter().rev().take(12).rev() {
        lines.push(Line::from_spans(vec![
            Span::styled(
                format!("{}: ", line.speaker),
                Style::new()
                    .fg(Color::BrightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(line.text.clone()),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::raw("Conversation is starting…"));
    }
    TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::Character)
        .style(Style::new().bg(Color::Rgb(18, 14, 28)).fg(Color::White))
        .render(rows[0], frame);
    TextBlock::new(Text::from_lines(vec![
        Line::from_spans(vec![
            Span::styled("You: ", Style::new().fg(Color::BrightCyan)),
            Span::raw(conversation.input.clone()),
        ]),
        Line::raw(conversation.status.clone()),
    ]))
    .wrap(TextWrap::Character)
    .style(
        Style::new()
            .bg(Color::Rgb(24, 18, 36))
            .fg(Color::BrightWhite),
    )
    .render(rows[1], frame);
    frame.write_line_with_fallback_style(
        rows[2],
        &Line::raw(format!(
            "Enter sends · Esc returns to office · d CEO dashboard · session {}",
            conversation.handle.session
        )),
        Style::new()
            .bg(Color::Rgb(18, 14, 28))
            .fg(Color::BrightBlack),
    );
}

fn render_blims_help_modal(area: Rect, frame: &mut Frame<'_>) {
    let modal = centered(area, Size::new(70, 10));
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightBlue)))
        .title(" Blims controls ")
        .background(Style::new().bg(Color::Rgb(18, 20, 38)));
    panel.render(modal, frame);
    let text = Text::from_lines(vec![
        Line::raw("arrows/hjkl walk one tile around the top-down office"),
        Line::raw("e/enter uses the selected nearby interaction"),
        Line::raw("t opens native in-game coworker chat; tab cycles interactions"),
        Line::raw("d opens CEO dashboard with initiatives, tasks, proposals, artifacts"),
        Line::raw("w opens starter office picker; escape closes modals"),
        Line::raw("r refreshes the morning report"),
        Line::raw("?/q toggle help / exit the office"),
    ]);
    TextBlock::new(text)
        .wrap(TextWrap::Character)
        .style(
            Style::new()
                .bg(Color::Rgb(18, 20, 38))
                .fg(Color::BrightWhite),
        )
        .render(panel.inner_area(modal).inset(Insets::all(1)), frame);
}

fn color_from_name(name: &str) -> Color {
    match name {
        "yellow" => Color::Yellow,
        "bright-white" => Color::BrightWhite,
        "cyan" => Color::Cyan,
        "magenta" => Color::Magenta,
        "green" => Color::Green,
        "blue" => Color::Blue,
        _ => Color::White,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BlimsTilePosition {
    x: i64,
    y: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlimsTileRect {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

impl BlimsTileRect {
    const fn right(self) -> i64 {
        self.x + self.width - 1
    }

    const fn bottom(self) -> i64 {
        self.y + self.height - 1
    }

    const fn contains(self, position: BlimsTilePosition) -> bool {
        position.x >= self.x
            && position.x <= self.right()
            && position.y >= self.y
            && position.y <= self.bottom()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsWorldGeometry {
    corridors: BTreeSet<BlimsTilePosition>,
    walkable: BTreeSet<BlimsTilePosition>,
    tile_rooms: BTreeMap<BlimsTilePosition, String>,
    world_width: i64,
    world_height: i64,
}

impl BlimsWorldGeometry {
    fn from_world(world: &BlimsWorldSnapshot) -> Self {
        let corridors = corridor_tiles(world);
        let mut walkable = corridors.clone();
        let mut tile_rooms = BTreeMap::new();
        for room in &world.rooms {
            let rect = room_tile_rect(room);
            for y in rect.y..=rect.bottom() {
                for x in rect.x..=rect.right() {
                    let tile = BlimsTilePosition { x, y };
                    walkable.insert(tile);
                    tile_rooms.insert(tile, room.id.clone());
                }
            }
        }
        Self {
            corridors,
            walkable,
            tile_rooms,
            world_width: world_tile_width(world),
            world_height: world_tile_height(world),
        }
    }

    fn room_at<'a>(
        &self,
        world: &'a BlimsWorldSnapshot,
        tile: BlimsTilePosition,
    ) -> Option<&'a BlimsRoomSnapshot> {
        let room_id = self.tile_rooms.get(&tile)?;
        world.rooms.iter().find(|room| room.id == *room_id)
    }

    fn room_id_at(&self, tile: BlimsTilePosition) -> Option<String> {
        self.tile_rooms.get(&tile).cloned()
    }

    fn is_walkable(&self, tile: BlimsTilePosition) -> bool {
        self.walkable.contains(&tile)
    }

    fn next_walkable_step(&self, sprite: &mut BlimsAgentSpriteState) -> BlimsTilePosition {
        if sprite.position == sprite.target {
            sprite.path.clear();
            return sprite.position;
        }

        if sprite
            .path
            .front()
            .is_none_or(|step| *step != sprite.position)
        {
            sprite.path = self.path_between(sprite.position, sprite.target);
        }

        if sprite
            .path
            .front()
            .is_some_and(|step| *step == sprite.position)
        {
            sprite.path.pop_front();
        }

        sprite.path.front().copied().unwrap_or(sprite.position)
    }

    fn path_between(
        &self,
        start: BlimsTilePosition,
        target: BlimsTilePosition,
    ) -> VecDeque<BlimsTilePosition> {
        if start == target {
            return VecDeque::from([start]);
        }
        let start = self.nearest_walkable_tile(start);
        let target = self.nearest_walkable_tile(target);
        let mut frontier = VecDeque::from([start]);
        let mut previous = BTreeMap::from([(start, start)]);
        while let Some(current) = frontier.pop_front() {
            if current == target {
                break;
            }
            for next in self.walkable_neighbors(current) {
                if previous.contains_key(&next) {
                    continue;
                }
                frontier.push_back(next);
                previous.insert(next, current);
            }
        }
        if !previous.contains_key(&target) {
            return VecDeque::new();
        }
        let mut reversed = vec![target];
        let mut current = target;
        while current != start {
            current = previous[&current];
            reversed.push(current);
        }
        reversed.reverse();
        reversed.into_iter().collect()
    }

    fn walkable_neighbors(
        &self,
        tile: BlimsTilePosition,
    ) -> impl Iterator<Item = BlimsTilePosition> + '_ {
        [
            BlimsTilePosition {
                x: tile.x,
                y: tile.y - 1,
            },
            BlimsTilePosition {
                x: tile.x + 1,
                y: tile.y,
            },
            BlimsTilePosition {
                x: tile.x,
                y: tile.y + 1,
            },
            BlimsTilePosition {
                x: tile.x - 1,
                y: tile.y,
            },
        ]
        .into_iter()
        .filter(|candidate| self.is_walkable(*candidate))
    }

    fn nearest_walkable_tile(&self, tile: BlimsTilePosition) -> BlimsTilePosition {
        if self.is_walkable(tile) {
            return tile;
        }
        self.walkable
            .iter()
            .min_by_key(|candidate| manhattan_distance(**candidate, tile))
            .copied()
            .unwrap_or(tile)
    }

    fn nearest_walkable_tile_in_room(
        &self,
        tile: BlimsTilePosition,
        room_id: &str,
    ) -> BlimsTilePosition {
        if self.tile_rooms.get(&tile).is_some_and(|id| id == room_id) {
            return tile;
        }
        self.tile_rooms
            .iter()
            .filter(|(_, id)| id.as_str() == room_id)
            .map(|(candidate, _)| *candidate)
            .min_by_key(|candidate| manhattan_distance(*candidate, tile))
            .unwrap_or_else(|| self.nearest_walkable_tile(tile))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlimsViewport {
    origin: BlimsTilePosition,
}

impl BlimsViewport {
    fn for_area(geometry: &BlimsWorldGeometry, player: BlimsTilePosition, area: Rect) -> Self {
        let world_width = geometry.world_width;
        let world_height = geometry.world_height;
        let visible_width = i64::from(area.width).max(1);
        let visible_height = i64::from(area.height).max(1);
        let max_x = (world_width - visible_width).max(0);
        let max_y = (world_height - visible_height).max(0);
        Self {
            origin: BlimsTilePosition {
                x: (player.x - visible_width / 2).clamp(0, max_x),
                y: (player.y - visible_height / 2).clamp(0, max_y),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlimsAgentSpriteState {
    position: BlimsTilePosition,
    target: BlimsTilePosition,
    path: VecDeque<BlimsTilePosition>,
}

impl BlimsAgentSpriteState {
    const fn new(position: BlimsTilePosition) -> Self {
        Self {
            position,
            target: position,
            path: VecDeque::new(),
        }
    }
}

fn keep_conversation_agent_in_place(
    previous_world: &BlimsWorldSnapshot,
    next_world: &mut BlimsWorldSnapshot,
    sprites: &mut BTreeMap<String, BlimsAgentSpriteState>,
    agent_id: &str,
) {
    if let Some(previous_agent) = previous_world
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        && let Some(next_agent) = next_world
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_id)
    {
        next_agent.room_id.clone_from(&previous_agent.room_id);
    }
    if let Some(sprite) = sprites.get_mut(agent_id) {
        sprite.target = sprite.position;
        sprite.path.clear();
    }
}

fn initial_agent_sprites(
    world: &BlimsWorldSnapshot,
    geometry: &BlimsWorldGeometry,
) -> BTreeMap<String, BlimsAgentSpriteState> {
    world
        .agents
        .iter()
        .map(|agent| {
            let position = agent_tile(world, geometry, agent);
            (agent.id.clone(), BlimsAgentSpriteState::new(position))
        })
        .collect()
}

fn room_tile_rect(room: &BlimsRoomSnapshot) -> BlimsTileRect {
    BlimsTileRect {
        x: room.x.max(0),
        y: room.y.max(0),
        width: 12,
        height: 7,
    }
}

fn room_anchor(room: &BlimsRoomSnapshot) -> BlimsTilePosition {
    let rect = room_tile_rect(room);
    BlimsTilePosition {
        x: rect.x + rect.width / 2,
        y: rect.y + rect.height / 2,
    }
}

fn player_tile_for_room(world: &BlimsWorldSnapshot, room_id: &str) -> BlimsTilePosition {
    world
        .rooms
        .iter()
        .find(|room| room.id == room_id)
        .map_or(BlimsTilePosition { x: 1, y: 1 }, room_anchor)
}

fn agent_tile(
    world: &BlimsWorldSnapshot,
    geometry: &BlimsWorldGeometry,
    agent: &BlimsAgentSnapshot,
) -> BlimsTilePosition {
    let base = player_tile_for_room(world, &agent.room_id);
    let offset = stable_tile_offset(&agent.id);
    let preferred = BlimsTilePosition {
        x: base.x + offset.x,
        y: base.y + offset.y,
    };
    geometry.nearest_walkable_tile_in_room(preferred, &agent.room_id)
}

fn stable_tile_offset(id: &str) -> BlimsTilePosition {
    let sum = id.bytes().fold(0_u8, u8::wrapping_add);
    match sum % 5 {
        0 => BlimsTilePosition { x: -2, y: 0 },
        1 => BlimsTilePosition { x: 2, y: 0 },
        2 => BlimsTilePosition { x: 0, y: -1 },
        3 => BlimsTilePosition { x: 0, y: 1 },
        _ => BlimsTilePosition { x: 1, y: 1 },
    }
}

fn is_near_current_room(
    world: &BlimsWorldSnapshot,
    player: BlimsTilePosition,
    room_id: &str,
) -> bool {
    world.rooms.iter().any(|room| {
        room.id == room_id
            && (room_tile_rect(room).contains(player)
                || manhattan_distance(player, room_anchor(room)) <= 2)
    })
}

const fn manhattan_distance(a: BlimsTilePosition, b: BlimsTilePosition) -> i64 {
    (a.x - b.x).abs() + (a.y - b.y).abs()
}

fn corridor_tiles(world: &BlimsWorldSnapshot) -> BTreeSet<BlimsTilePosition> {
    let mut tiles = BTreeSet::new();
    let points = world.rooms.iter().map(room_anchor).collect::<Vec<_>>();
    for window in points.windows(2) {
        let [from, to] = window else { continue };
        for x in from.x.min(to.x)..=from.x.max(to.x) {
            tiles.insert(BlimsTilePosition { x, y: from.y });
        }
        for y in from.y.min(to.y)..=from.y.max(to.y) {
            tiles.insert(BlimsTilePosition { x: to.x, y });
        }
    }
    tiles
}

fn world_tile_width(world: &BlimsWorldSnapshot) -> i64 {
    world
        .rooms
        .iter()
        .map(|room| room_tile_rect(room).right() + 2)
        .max()
        .unwrap_or(world.width)
        .max(world.width)
        .max(1)
}

fn world_tile_height(world: &BlimsWorldSnapshot) -> i64 {
    world
        .rooms
        .iter()
        .map(|room| room_tile_rect(room).bottom() + 2)
        .max()
        .unwrap_or(world.height)
        .max(world.height)
        .max(1)
}

fn room_bg(room: &BlimsRoomSnapshot) -> Color {
    match room.room_kind.as_str() {
        "planning" | "strategy" => Color::Rgb(38, 31, 58),
        "engineering" => Color::Rgb(24, 38, 50),
        "creative" => Color::Rgb(44, 30, 50),
        "review" => Color::Rgb(28, 42, 34),
        _ => Color::Rgb(28, 31, 38),
    }
}

async fn start_blims_agent_talk_cli(agent_id: String) -> Result<(), CliError> {
    let handle = create_blims_agent_conversation(agent_id).await?;
    println!(
        "AI chat session with {}: {}",
        handle.agent_id, handle.session
    );
    println!("Attaching now. Press Ctrl-C to return to the Blims office.");
    attach_session(handle.session).await?;
    Ok(())
}

async fn create_blims_agent_conversation(
    agent_id: String,
) -> Result<BlimsConversationHandle, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": agent_id,
    });
    let response = call_blims_service("agent.talk_prompt", serde_json::to_vec(&request)?).await?;
    let prompt = decode_blims_response::<BlimsAgentTalkPrompt>(response)?;
    let session = create_agent_talk_session(&prompt).await?;
    Ok(BlimsConversationHandle {
        agent_id: prompt.agent_id,
        conversation_ref: prompt.conversation_id,
        session: session.id,
    })
}

async fn create_agent_talk_session(
    prompt: &BlimsAgentTalkPrompt,
) -> Result<bcode_session_models::SessionSummary, CliError> {
    let session = BcodeClient::default_endpoint()
        .create_session(Some(format!("Blims talk: {}", prompt.agent_id)))
        .await?;
    BcodeClient::default_endpoint()
        .send_user_message(session.id, prompt.prompt.clone())
        .await?;
    record_blims_conversation(prompt, &session.id).await?;
    Ok(session)
}

async fn record_blims_conversation(
    prompt: &BlimsAgentTalkPrompt,
    session_id: &SessionId,
) -> Result<(), CliError> {
    let _operation = submit_blims_command(&serde_json::json!({
        "type": "open_agent_conversation",
        "conversation_id": prompt.conversation_id,
        "agent_id": prompt.agent_id,
        "session_id": session_id.to_string(),
        "summary": "Bcode conversation session opened from Blims office.",
    }))
    .await?;
    Ok(())
}

async fn start_blims_task_work(task_id: String) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "task_id": task_id,
    });
    let response = call_blims_service("task.work_prompt", serde_json::to_vec(&request)?).await?;
    let prompt = decode_blims_response::<BlimsTaskWorkPrompt>(response)?;
    let worktree = create_blims_task_worktree(&prompt).await?;
    let session = worktree.session.as_ref().ok_or_else(|| {
        CliError::Blims("task worktree creation did not return a session".to_string())
    })?;
    BcodeClient::default_endpoint()
        .send_user_message(session.id, prompt.prompt.clone())
        .await?;
    println!(
        "task work session for {} as {}: {}",
        prompt.task_id, prompt.agent_id, session.id
    );
    println!("sandbox\t{}", worktree.path.display());
    if let Some(branch) = &worktree.branch {
        println!("branch\t{branch}");
        let proposal =
            register_blims_work_proposal(&prompt, &worktree, &session.id, branch).await?;
        println!("proposal\t{}", proposal.id);
    }
    println!("Attaching now. Press Ctrl-C to return.");
    attach_session(session.id).await?;
    Ok(())
}

async fn create_blims_task_worktree(
    prompt: &BlimsTaskWorkPrompt,
) -> Result<WorktreeCreateResponse, CliError> {
    BcodeClient::default_endpoint()
        .create_worktree(WorktreeCreateRequest {
            name: format!("blims-{}", prompt.task_id),
            cwd: None,
            path: None,
            branch: None,
            new_branch: Some(format!("blims/{}", prompt.task_id)),
            base_ref: Some(WorktreeBaseRef::Head),
            detach: false,
            force: false,
            attach_session_id: None,
            new_session: true,
            no_setup: false,
        })
        .await
        .map_err(Into::into)
}

async fn register_blims_work_proposal(
    prompt: &BlimsTaskWorkPrompt,
    worktree: &WorktreeCreateResponse,
    session_id: &SessionId,
    branch: &str,
) -> Result<BlimsWorkProposalSummary, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "task_id": prompt.task_id,
        "session_id": session_id.to_string(),
        "worktree_path": worktree.path,
        "branch": branch,
    });
    let response = call_blims_service("proposal.register", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<BlimsWorkProposalSummary>(response)
}

fn print_blims_world(world: &BlimsWorldSnapshot) {
    println!(
        "{} [{}] ({}×{})",
        world.theme, world.template_id, world.width, world.height
    );
    println!("player: {}", world.player_name);
    println!("rooms:");
    for room in &world.rooms {
        println!(
            "* {} {} ({}) at {},{} [{}] +{} {} - {}",
            room.symbol,
            room.name,
            room.id,
            room.x,
            room.y,
            room.color,
            room.productivity_modifier,
            room.room_kind,
            room.purpose
        );
    }
    println!("agents:");
    for agent in &world.agents {
        println!(
            "* {} ({}) at {} - {}",
            agent.name, agent.role, agent.room_id, agent.status
        );
    }
}

fn next_room_id(world: &BlimsWorldSnapshot, current_room_id: &str) -> String {
    if world.rooms.is_empty() {
        return current_room_id.to_string();
    }
    let current = room_index(world, current_room_id);
    world.rooms[(current + 1) % world.rooms.len()].id.clone()
}

fn previous_room_id(world: &BlimsWorldSnapshot, current_room_id: &str) -> String {
    if world.rooms.is_empty() {
        return current_room_id.to_string();
    }
    let current = room_index(world, current_room_id);
    let previous = current
        .checked_sub(1)
        .unwrap_or_else(|| world.rooms.len().saturating_sub(1));
    world.rooms[previous].id.clone()
}

fn room_index(world: &BlimsWorldSnapshot, current_room_id: &str) -> usize {
    world
        .rooms
        .iter()
        .position(|room| room.id == current_room_id)
        .unwrap_or_default()
}

trait PrintableReport {
    fn title(&self) -> &str;
    fn bullets(&self) -> &[String];
}

impl PrintableReport for BlimsMorningReport {
    fn title(&self) -> &str {
        &self.title
    }

    fn bullets(&self) -> &[String] {
        &self.bullets
    }
}

impl PrintableReport for BlimsFocusedReport {
    fn title(&self) -> &str {
        &self.title
    }

    fn bullets(&self) -> &[String] {
        &self.bullets
    }
}

fn print_report_response<T>(
    response: bcode_ipc::PluginServiceResponse,
    json: bool,
) -> Result<(), CliError>
where
    T: PrintableReport + for<'de> Deserialize<'de>,
{
    if json {
        print_blims_service_response(response);
    } else {
        let report = decode_blims_response::<T>(response)?;
        println!("{}", report.title());
        for bullet in report.bullets() {
            println!("* {bullet}");
        }
    }
    Ok(())
}

async fn print_agent_permission(agent_id: &str, json: bool) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": agent_id,
    });
    let response =
        call_blims_service("agent.get_permission", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let permission = decode_blims_response::<BlimsAgentPermission>(response)?;
        println!("agent: {}", permission.agent_id);
        println!("bcode agent: {}", permission.bcode_agent_id);
        println!("bash: {}", permission.bash);
        println!("read: {}", permission.read);
        println!("write: {}", permission.write);
        println!("edit: {}", permission.edit);
        println!("external_directory: {}", permission.external_directory);
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
async fn set_agent_permission(
    agent_id: String,
    bcode_agent_id: Option<String>,
    bash: Option<String>,
    read: Option<String>,
    write: Option<String>,
    edit: Option<String>,
    external_directory: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let current = load_agent_permission(&agent_id).await?;
    let request = BlimsAgentPermission {
        agent_id,
        bcode_agent_id: bcode_agent_id.unwrap_or(current.bcode_agent_id),
        bash: bash.unwrap_or(current.bash),
        read: read.unwrap_or(current.read),
        write: write.unwrap_or(current.write),
        edit: edit.unwrap_or(current.edit),
        external_directory: external_directory.unwrap_or(current.external_directory),
    };
    let payload = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": request.agent_id,
        "bcode_agent_id": request.bcode_agent_id,
        "bash": request.bash,
        "read": request.read,
        "write": request.write,
        "edit": request.edit,
        "external_directory": request.external_directory,
    });
    let response =
        call_blims_service("agent.set_permission", serde_json::to_vec(&payload)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let permission = decode_blims_response::<BlimsAgentPermission>(response)?;
        println!(
            "permission updated: {} -> bash {}, read {}, write {}, edit {}, external {}",
            permission.agent_id,
            permission.bash,
            permission.read,
            permission.write,
            permission.edit,
            permission.external_directory
        );
    }
    Ok(())
}

async fn load_agent_permission(agent_id: &str) -> Result<BlimsAgentPermission, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": agent_id,
    });
    let response =
        call_blims_service("agent.get_permission", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<BlimsAgentPermission>(response)
}
async fn print_world_templates(json: bool) -> Result<(), CliError> {
    let response = call_blims_service("world.template_list", blims_workspace_payload()?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let templates = decode_blims_response::<Vec<BlimsWorldTemplateSummary>>(response)?;
        for template in templates {
            println!(
                "{}\t{}\t{} rooms\t{}\t{}",
                template.id, template.name, template.rooms, template.flavor, template.description
            );
        }
    }
    Ok(())
}

async fn select_world_template_state(
    template_id: String,
) -> Result<(BlimsWorldSelection, bcode_ipc::PluginServiceResponse), CliError> {
    let response = call_blims_service(
        "command.submit",
        serde_json::to_vec(&blims_command_request(&serde_json::json!({
            "type": "select_world_template",
            "template_id": template_id,
        }))?)?,
    )
    .await?;
    let world = load_blims_world().await?;
    let interactions = load_blims_interactions().await?;
    let report = load_blims_report().await?;
    Ok((
        BlimsWorldSelection {
            world,
            report,
            interactions,
        },
        response,
    ))
}

async fn select_world_template(template_id: String, json: bool) -> Result<(), CliError> {
    let (selection, response) = select_world_template_state(template_id).await?;
    if json {
        print_blims_service_response(response);
    } else {
        println!(
            "selected world: {} ({})",
            selection.world.theme, selection.world.template_id
        );
        print_blims_world(&selection.world);
    }
    Ok(())
}

async fn print_blims_status(json: bool) -> Result<(), CliError> {
    let response = call_blims_service("company.status", blims_workspace_payload()?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let status = decode_blims_response::<BlimsCompanyStatus>(response)?;
        println!("Blims: {}", status.state);
        println!("{}", status.message);
        println!("daemon connected: {}", status.daemon_connected);
        println!("lifecycle: {}", status.lifecycle_status);
        println!("state root: {}", status.state_root.display());
        println!("database: {}", status.database_path.display());
        println!("daemon namespace: {}", status.daemon_namespace);
        println!("daemon metadata: {}", status.daemon_metadata_path.display());
        println!("compatibility: {}", status.compatibility);
    }
    Ok(())
}

async fn create_blims_company(json: bool) -> Result<(), CliError> {
    let response = call_blims_service("company.create", blims_workspace_payload()?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let status = decode_blims_response::<BlimsCompanyStatus>(response)?;
        println!("Blims company created");
        println!("state root: {}", status.state_root.display());
        println!("database: {}", status.database_path.display());
        println!("daemon namespace: {}", status.daemon_namespace);
        println!("daemon metadata: {}", status.daemon_metadata_path.display());
    }
    Ok(())
}

async fn print_agent_service_result(
    operation: &str,
    agent_id: &str,
    json: bool,
) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": agent_id,
    });
    let response = call_blims_service(operation, serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let agent = decode_blims_response::<BlimsAgentSnapshot>(response)?;
        println!(
            "{} ({}) at {} — {}",
            agent.name, agent.role, agent.room_id, agent.status
        );
    }
    Ok(())
}

async fn hire_blims_agent(
    agent_id: String,
    name: String,
    role: String,
    room_id: String,
    json: bool,
) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": agent_id,
        "name": name,
        "role": role,
        "room_id": room_id,
    });
    let response = call_blims_service("agent.hire", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let agent = decode_blims_response::<BlimsAgentSnapshot>(response)?;
        println!("agent hired: {} ({})", agent.name, agent.id);
    }
    Ok(())
}

async fn print_company_lifecycle_update(
    operation: &str,
    message: &str,
    json: bool,
) -> Result<(), CliError> {
    let response = call_blims_service(operation, blims_workspace_payload()?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let status = decode_blims_response::<BlimsCompanyStatus>(response)?;
        println!("{message}");
        println!("lifecycle: {}", status.lifecycle_status);
        println!("state root: {}", status.state_root.display());
    }
    Ok(())
}

async fn handle_blims_ai_work_command(command: BlimsAiWorkCommand) -> Result<(), CliError> {
    match command {
        BlimsAiWorkCommand::List { limit, json } => print_blims_ai_work(limit, json).await,
        BlimsAiWorkCommand::Start { work_id } => start_blims_prepared_ai_work(work_id).await,
    }
}

async fn print_blims_ai_work(limit: u64, json: bool) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "limit": limit,
    });
    let response =
        call_blims_service("projection.ai_work.get", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let work = decode_blims_response::<Vec<BlimsPreparedAiWorkItem>>(response)?;
        if work.is_empty() {
            println!("no prepared AI work yet");
        }
        for item in work {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                item.id,
                item.kind,
                item.agent_id,
                item.task_id.unwrap_or_default(),
                item.operation_id
            );
        }
    }
    Ok(())
}

async fn start_blims_prepared_ai_work(work_id: String) -> Result<(), CliError> {
    let (work, session) = create_blims_prepared_ai_work_session(&work_id).await?;
    println!("started AI work session: {}", session.id);
    println!("work: {}", work.id);
    println!("kind: {}", work.kind);
    println!("agent: {}", work.agent_id);
    attach_session(session.id).await?;
    Ok(())
}

async fn start_blims_prepared_ai_work_detached(work_id: String) -> Result<(), CliError> {
    let (_work, _session) = create_blims_prepared_ai_work_session(&work_id).await?;
    Ok(())
}

async fn create_blims_prepared_ai_work_session(
    work_id: &str,
) -> Result<
    (
        BlimsPreparedAiWorkItem,
        bcode_session_models::SessionSummary,
    ),
    CliError,
> {
    let work = load_blims_prepared_ai_work(work_id).await?;
    let session = BcodeClient::default_endpoint()
        .create_session(Some(format!("Blims AI work: {}", work.id)))
        .await?;
    BcodeClient::default_endpoint()
        .send_user_message(session.id, work.prompt.clone())
        .await?;
    Ok((work, session))
}

async fn load_blims_prepared_ai_work(work_id: &str) -> Result<BlimsPreparedAiWorkItem, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "limit": 250_u64,
    });
    let response =
        call_blims_service("projection.ai_work.get", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<Vec<BlimsPreparedAiWorkItem>>(response)?
        .into_iter()
        .find(|work| work.id == work_id)
        .ok_or_else(|| CliError::Blims(format!("unknown prepared AI work: {work_id}")))
}

async fn handle_blims_task_command(command: BlimsTaskCommand) -> Result<(), CliError> {
    match command {
        BlimsTaskCommand::List { json } => {
            let response = call_blims_service("task.list", blims_workspace_payload()?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                let tasks = decode_blims_response::<Vec<BlimsTaskSummary>>(response)?;
                for task in tasks {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        task.id,
                        task.initiative_id,
                        task.priority,
                        task.status,
                        task.assigned_agent_id,
                        task.title,
                        task.description,
                        task.rationale
                    );
                }
            }
        }
        BlimsTaskCommand::Inspect { task_id, json } => {
            let request = serde_json::json!({
                "working_directory": std::env::current_dir()?,
                "task_id": task_id,
            });
            let response =
                call_blims_service("task.inspect", serde_json::to_vec(&request)?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                print_task_detail(&decode_blims_response::<BlimsTaskSummary>(response)?);
            }
        }
        BlimsTaskCommand::Work { task_id } => {
            start_blims_task_work(task_id).await?;
        }
    }
    Ok(())
}

async fn handle_blims_artifact_command(command: BlimsArtifactCommand) -> Result<(), CliError> {
    match command {
        BlimsArtifactCommand::List { json } => {
            let response = call_blims_service("artifact.list", blims_workspace_payload()?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                let artifacts = decode_blims_response::<Vec<BlimsArtifactSummary>>(response)?;
                for artifact in artifacts {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        artifact.id,
                        artifact.initiative_id,
                        artifact.kind,
                        artifact.status,
                        artifact.title
                    );
                }
            }
        }
        BlimsArtifactCommand::Inspect { artifact_id, json } => {
            let request = serde_json::json!({
                "working_directory": std::env::current_dir()?,
                "artifact_id": artifact_id,
            });
            let response =
                call_blims_service("artifact.inspect", serde_json::to_vec(&request)?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                print_artifact_detail(&decode_blims_response::<BlimsArtifactDetail>(response)?);
            }
        }
        BlimsArtifactCommand::Apply { artifact_id, yes } => {
            apply_blims_patch_artifact(artifact_id, yes).await?;
        }
        BlimsArtifactCommand::Create {
            kind,
            title,
            initiative_id,
            payload,
            file,
            json,
        } => {
            create_blims_artifact(kind, title, initiative_id, payload, file, json).await?;
        }
        BlimsArtifactCommand::Approve { artifact_id, json } => {
            print_artifact_status_update(
                "artifact.approve",
                "artifact approved",
                artifact_id,
                json,
            )
            .await?;
        }
        BlimsArtifactCommand::Reject { artifact_id, json } => {
            print_artifact_status_update("artifact.reject", "artifact rejected", artifact_id, json)
                .await?;
        }
        BlimsArtifactCommand::Defer { artifact_id, json } => {
            print_artifact_status_update("artifact.defer", "artifact deferred", artifact_id, json)
                .await?;
        }
    }
    Ok(())
}

async fn handle_blims_proposal_command(command: BlimsProposalCommand) -> Result<(), CliError> {
    match command {
        BlimsProposalCommand::List { json } => {
            let response = call_blims_service("proposal.list", blims_workspace_payload()?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                print_proposal_list(&decode_blims_response::<Vec<BlimsWorkProposalSummary>>(
                    response,
                )?);
            }
        }
        BlimsProposalCommand::Inspect { proposal_id, json } => {
            let proposal = load_blims_proposal("proposal.inspect", proposal_id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&proposal)?);
            } else {
                print_proposal_detail(&proposal);
            }
        }
        BlimsProposalCommand::MarkReady { proposal_id, json } => {
            print_proposal_status_update(
                "proposal.mark_ready",
                "proposal ready for review",
                proposal_id,
                json,
            )
            .await?;
        }
        BlimsProposalCommand::Approve { proposal_id, json } => {
            print_proposal_status_update(
                "proposal.approve",
                "proposal approved",
                proposal_id,
                json,
            )
            .await?;
        }
        BlimsProposalCommand::Reject { proposal_id, json } => {
            print_proposal_status_update("proposal.reject", "proposal rejected", proposal_id, json)
                .await?;
        }
        BlimsProposalCommand::Defer { proposal_id, json } => {
            print_proposal_status_update("proposal.defer", "proposal deferred", proposal_id, json)
                .await?;
        }
        BlimsProposalCommand::Patch { proposal_id, json } => {
            create_blims_proposal_patch(proposal_id, json).await?;
        }
    }
    Ok(())
}

async fn create_blims_artifact(
    kind: String,
    title: String,
    initiative_id: String,
    payload: Option<String>,
    file: bool,
    json: bool,
) -> Result<(), CliError> {
    let payload_json = match payload {
        Some(payload) if file => std::fs::read_to_string(payload)?,
        Some(payload) => payload,
        None => "{}".to_string(),
    };
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "initiative_id": initiative_id,
        "kind": kind,
        "title": title,
        "payload_json": payload_json,
    });
    let response = call_blims_service("artifact.create", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let artifact = decode_blims_response::<BlimsArtifactDetail>(response)?;
        println!("artifact created: {} ({})", artifact.title, artifact.id);
        print_artifact_detail(&artifact);
    }
    Ok(())
}

async fn print_artifact_status_update(
    operation: &str,
    message: &str,
    artifact_id: String,
    json: bool,
) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "artifact_id": artifact_id,
    });
    let response = call_blims_service(operation, serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let artifact = decode_blims_response::<BlimsArtifactDetail>(response)?;
        println!("{message}: {}", artifact.id);
        print_artifact_detail(&artifact);
    }
    Ok(())
}

async fn update_blims_proposal_status(
    operation: &str,
    proposal_id: String,
) -> Result<BlimsWorkProposalSummary, CliError> {
    let status = status_from_legacy_operation(operation)?;
    let _operation = submit_blims_command(&serde_json::json!({
        "type": "set_proposal_status",
        "proposal_id": proposal_id,
        "status": status,
    }))
    .await?;
    let dashboard = load_ceo_dashboard_projection().await?;
    dashboard
        .proposals
        .into_iter()
        .find(|proposal| proposal.id == proposal_id)
        .ok_or_else(|| CliError::Blims(format!("unknown proposal: {proposal_id}")))
}

async fn update_blims_artifact_status(
    operation: &str,
    artifact_id: String,
) -> Result<BlimsArtifactDetail, CliError> {
    let status = status_from_legacy_operation(operation)?;
    let _operation = submit_blims_command(&serde_json::json!({
        "type": "set_artifact_status",
        "artifact_id": artifact_id,
        "status": status,
    }))
    .await?;
    load_blims_artifact(artifact_id).await
}

fn status_from_legacy_operation(operation: &str) -> Result<&'static str, CliError> {
    match operation.rsplit('.').next() {
        Some("approve") => Ok("approved"),
        Some("reject") => Ok("rejected"),
        Some("defer") => Ok("deferred"),
        _ => Err(CliError::Blims(format!(
            "unsupported status operation: {operation}"
        ))),
    }
}

async fn print_proposal_status_update(
    operation: &str,
    message: &str,
    proposal_id: String,
    json: bool,
) -> Result<(), CliError> {
    let proposal = load_blims_proposal(operation, proposal_id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&proposal)?);
    } else {
        println!("{message}: {}", proposal.id);
        print_proposal_detail(&proposal);
    }
    Ok(())
}

async fn create_blims_proposal_patch(proposal_id: String, json: bool) -> Result<(), CliError> {
    let proposal = load_blims_proposal("proposal.inspect", proposal_id).await?;
    let patch = git_diff_for_proposal(&proposal)?;
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "proposal_id": proposal.id,
        "patch": patch,
    });
    let response =
        call_blims_service("proposal.record_patch", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let artifact = decode_blims_response::<BlimsArtifactDetail>(response)?;
        println!("patch artifact created: {}", artifact.id);
        println!("initiative: {}", artifact.initiative_id);
    }
    Ok(())
}

async fn apply_blims_patch_artifact(artifact_id: String, yes: bool) -> Result<(), CliError> {
    let artifact = load_blims_artifact(artifact_id).await?;
    let patch = patch_from_artifact(&artifact)?;
    if patch.trim().is_empty() {
        return Err(CliError::Blims(format!(
            "artifact {} contains an empty patch",
            artifact.id
        )));
    }
    if !yes && !confirm_patch_apply(&artifact)? {
        println!("apply cancelled");
        return Ok(());
    }
    apply_patch_to_current_worktree(&patch)?;
    println!("applied patch artifact: {}", artifact.id);
    Ok(())
}

async fn load_blims_artifact(artifact_id: String) -> Result<BlimsArtifactDetail, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "artifact_id": artifact_id,
    });
    let response = call_blims_service("artifact.inspect", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<BlimsArtifactDetail>(response)
}

fn patch_from_artifact(artifact: &BlimsArtifactDetail) -> Result<String, CliError> {
    if artifact.kind != "proposal_patch" {
        return Err(CliError::Blims(format!(
            "artifact {} is {}, not proposal_patch",
            artifact.id, artifact.kind
        )));
    }
    let payload = serde_json::from_str::<serde_json::Value>(&artifact.payload_json)?;
    payload
        .get("patch")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| CliError::Blims(format!("artifact {} has no patch payload", artifact.id)))
}

fn confirm_patch_apply(artifact: &BlimsArtifactDetail) -> Result<bool, CliError> {
    println!(
        "About to apply patch artifact {} to the current worktree.",
        artifact.id
    );
    println!(
        "This can modify files under {}.",
        std::env::current_dir()?.display()
    );
    print!("Type `apply` to continue: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim() == "apply")
}

fn apply_patch_to_current_worktree(patch: &str) -> Result<(), CliError> {
    let mut child = Command::new("git")
        .arg("apply")
        .arg("--3way")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| CliError::Blims("failed to open stdin for git apply".to_string()))?;
    stdin.write_all(patch.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(CliError::Blims(format!(
            "git apply failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn git_diff_for_proposal(proposal: &BlimsWorkProposalSummary) -> Result<String, CliError> {
    let output = Command::new("git")
        .arg("diff")
        .arg("HEAD")
        .current_dir(&proposal.worktree_path)
        .output()?;
    if !output.status.success() {
        return Err(CliError::Blims(format!(
            "failed to create git diff for proposal {}: {}",
            proposal.id,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn load_blims_proposal(
    operation: &str,
    proposal_id: String,
) -> Result<BlimsWorkProposalSummary, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "proposal_id": proposal_id,
    });
    let response = call_blims_service(operation, serde_json::to_vec(&request)?).await?;
    decode_blims_response::<BlimsWorkProposalSummary>(response)
}

fn print_task_detail(task: &BlimsTaskSummary) {
    println!("{}", task.title);
    println!("id: {}", task.id);
    println!("initiative: {}", task.initiative_id);
    println!("status: {}", task.status);
    println!("priority: {}", task.priority);
    println!("assigned agent: {}", task.assigned_agent_id);
    println!("description: {}", task.description);
    println!("rationale: {}", task.rationale);
}

fn print_artifact_detail(artifact: &BlimsArtifactDetail) {
    println!("{}", artifact.title);
    println!("id: {}", artifact.id);
    println!("initiative: {}", artifact.initiative_id);
    println!("kind: {}", artifact.kind);
    println!("status: {}", artifact.status);
    println!("payload:");
    println!("{}", artifact.payload_json);
}

fn print_proposal_list(proposals: &[BlimsWorkProposalSummary]) {
    if proposals.is_empty() {
        println!("no work proposals yet");
        return;
    }
    for proposal in proposals {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            proposal.id,
            proposal.task_id,
            proposal.status,
            proposal.agent_id,
            proposal.branch,
            proposal.summary
        );
    }
}

fn print_proposal_detail(proposal: &BlimsWorkProposalSummary) {
    println!("{}", proposal.id);
    println!("task: {}", proposal.task_id);
    println!("initiative: {}", proposal.initiative_id);
    println!("agent: {}", proposal.agent_id);
    println!("session: {}", proposal.session_id);
    println!("worktree: {}", proposal.worktree_path);
    println!("branch: {}", proposal.branch);
    println!("status: {}", proposal.status);
    println!("summary: {}", proposal.summary);
    println!("validation: {}", proposal.validation_notes);
}

fn print_initiative_list(initiatives: &[BlimsInitiativeSummary]) {
    if initiatives.is_empty() {
        println!("no initiatives yet");
        return;
    }
    for initiative in initiatives {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            initiative.id,
            initiative.priority,
            initiative.status,
            initiative.title,
            initiative.description
        );
    }
}

fn print_initiative_detail(initiative: &BlimsInitiativeSummary) {
    println!("{}", initiative.title);
    println!("id: {}", initiative.id);
    println!("status: {}", initiative.status);
    println!("priority: {}", initiative.priority);
    println!("description: {}", initiative.description);
}

async fn handle_blims_initiative_command(command: BlimsInitiativeCommand) -> Result<(), CliError> {
    match command {
        BlimsInitiativeCommand::Create {
            title,
            description,
            priority,
            json,
        } => create_blims_initiative(title, description, priority, json).await?,
        BlimsInitiativeCommand::List { json } => list_blims_initiatives(json).await?,
        BlimsInitiativeCommand::Inspect {
            initiative_id,
            json,
        } => inspect_blims_initiative(&initiative_id, json).await?,
        BlimsInitiativeCommand::SetGuidance {
            initiative_id,
            guidance,
            strength,
            json,
        } => set_blims_initiative_guidance(initiative_id, guidance, strength, json).await?,
        BlimsInitiativeCommand::Pause {
            initiative_id,
            json,
        } => {
            print_initiative_status_update("initiative.pause", &initiative_id, json).await?;
        }
        BlimsInitiativeCommand::Resume {
            initiative_id,
            json,
        } => {
            print_initiative_status_update("initiative.resume", &initiative_id, json).await?;
        }
        BlimsInitiativeCommand::PlanPrompt { initiative_id } => {
            let prompt = blims_initiative_plan_prompt(initiative_id).await?;
            println!("# Initiative {} AI planning prompt", prompt.initiative_id);
            println!("{}", prompt.prompt);
        }
        BlimsInitiativeCommand::Plan { initiative_id } => {
            start_blims_initiative_plan(initiative_id).await?;
        }
        BlimsInitiativeCommand::ImportPlan {
            initiative_id,
            plan,
            file,
        } => {
            import_blims_initiative_plan(initiative_id, plan, file).await?;
        }
    }
    Ok(())
}

async fn create_blims_initiative(
    title: String,
    description: Option<String>,
    priority: Option<i64>,
    json: bool,
) -> Result<(), CliError> {
    let request = BlimsInitiativeCreateRequest {
        working_directory: std::env::current_dir()?,
        title,
        description,
        priority,
    };
    let response = call_blims_service("initiative.create", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let initiative = decode_blims_response::<BlimsInitiativeSummary>(response)?;
        println!(
            "initiative created: {} ({})",
            initiative.title, initiative.id
        );
    }
    Ok(())
}

async fn list_blims_initiatives(json: bool) -> Result<(), CliError> {
    let response = call_blims_service("initiative.list", blims_workspace_payload()?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let initiatives = decode_blims_response::<Vec<BlimsInitiativeSummary>>(response)?;
        print_initiative_list(&initiatives);
    }
    Ok(())
}

async fn inspect_blims_initiative(initiative_id: &str, json: bool) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "initiative_id": initiative_id,
    });
    let response = call_blims_service("initiative.inspect", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        print_initiative_detail(&decode_blims_response::<BlimsInitiativeSummary>(response)?);
    }
    Ok(())
}

async fn set_blims_initiative_guidance(
    initiative_id: String,
    guidance: String,
    strength: String,
    json: bool,
) -> Result<(), CliError> {
    let request = BlimsInitiativeGuidanceRequest {
        working_directory: std::env::current_dir()?,
        initiative_id,
        guidance,
        strength,
    };
    let response =
        call_blims_service("initiative.set_guidance", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let guidance = decode_blims_response::<BlimsGuidanceSummary>(response)?;
        println!("initiative guidance set: {}", guidance.guidance);
    }
    Ok(())
}

async fn print_initiative_status_update(
    operation: &str,
    initiative_id: &str,
    json: bool,
) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "initiative_id": initiative_id,
    });
    let response = call_blims_service(operation, serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        print_initiative_detail(&decode_blims_response::<BlimsInitiativeSummary>(response)?);
    }
    Ok(())
}

async fn blims_initiative_plan_prompt(
    initiative_id: String,
) -> Result<BlimsPlanningPrompt, CliError> {
    let request = BlimsInitiativePlanPromptRequest {
        working_directory: std::env::current_dir()?,
        initiative_id,
    };
    let response =
        call_blims_service("initiative.plan_prompt", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<BlimsPlanningPrompt>(response)
}

async fn start_blims_initiative_plan(initiative_id: String) -> Result<(), CliError> {
    let prompt = blims_initiative_plan_prompt(initiative_id).await?;
    let session = BcodeClient::default_endpoint()
        .create_session(Some(format!("Blims plan: {}", prompt.initiative_id)))
        .await?;
    BcodeClient::default_endpoint()
        .send_user_message(session.id, prompt.prompt)
        .await?;
    println!("planning session: {}", session.id);
    println!(
        "When the AI returns JSON, import it with: bcode blims initiative import-plan {} '<json>'",
        prompt.initiative_id
    );
    println!(
        "Or save the JSON and run in-office: import plan {} <file>",
        prompt.initiative_id
    );
    println!("Attaching now. Press Ctrl-C to return.");
    attach_session(session.id).await?;
    Ok(())
}

async fn import_blims_initiative_plan(
    initiative_id: String,
    plan: String,
    file: bool,
) -> Result<(), CliError> {
    let plan_json = if file {
        std::fs::read_to_string(plan)?
    } else {
        plan
    };
    let request = BlimsInitiativeImportPlanRequest {
        working_directory: std::env::current_dir()?,
        initiative_id,
        plan: serde_json::from_str(&plan_json)?,
    };
    let response =
        call_blims_service("initiative.import_plan", serde_json::to_vec(&request)?).await?;
    print_blims_service_response(response);
    Ok(())
}

async fn handle_blims_guidance_command(command: BlimsGuidanceCommand) -> Result<(), CliError> {
    match command {
        BlimsGuidanceCommand::Set {
            guidance,
            strength,
            json,
        } => {
            let request = BlimsGuidanceSetRequest {
                working_directory: std::env::current_dir()?,
                guidance,
                strength,
            };
            let response =
                call_blims_service("guidance.set", serde_json::to_vec(&request)?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                let guidance = decode_blims_response::<BlimsGuidanceSummary>(response)?;
                println!(
                    "guidance set: {} ({})",
                    guidance.guidance, guidance.strength
                );
            }
        }
        BlimsGuidanceCommand::List { json } => {
            let response = call_blims_service("guidance.list", blims_workspace_payload()?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                let guidance = decode_blims_response::<Vec<BlimsGuidanceSummary>>(response)?;
                for item in guidance {
                    println!(
                        "{}\t{}\t{}\t{}",
                        item.id, item.strength, item.active, item.guidance
                    );
                }
            }
        }
    }
    Ok(())
}

fn blims_workspace_payload() -> Result<Vec<u8>, CliError> {
    let request = BlimsWorkspaceRequest {
        working_directory: std::env::current_dir()?,
    };
    Ok(serde_json::to_vec(&request)?)
}

async fn call_blims_service(
    operation: &str,
    payload: Vec<u8>,
) -> Result<bcode_ipc::PluginServiceResponse, CliError> {
    BcodeClient::default_endpoint()
        .call_plugin_service("bcode.blims/v1".to_string(), operation.to_string(), payload)
        .await
        .map_err(CliError::from)
}

fn decode_blims_response<T: for<'de> Deserialize<'de>>(
    response: bcode_ipc::PluginServiceResponse,
) -> Result<T, CliError> {
    if let Some(error) = response.error {
        return Err(CliError::Blims(format!(
            "{}: {}",
            error.code, error.message
        )));
    }
    Ok(serde_json::from_slice(&response.payload)?)
}

fn print_blims_service_response(response: bcode_ipc::PluginServiceResponse) {
    print_service_response(response);
}
