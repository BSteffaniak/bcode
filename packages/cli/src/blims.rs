#![allow(clippy::module_name_repetitions)]

use super::{CliError, attach_session, ensure_server_running, print_service_response};
use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bcode_worktree_models::{WorktreeBaseRef, WorktreeCreateRequest, WorktreeCreateResponse};
use bmux_keyboard::KeyCode;
use bmux_tui::layout::centered;
use bmux_tui::prelude::{
    Border, Color, Constraint, CrosstermTerminalGuard, Direction, Event, Frame, Insets, Line,
    Modifier, Panel, Point, Rect, Size, Span, Style, Terminal, Text, TextBlock, TextWrap, Widget,
    read_event, split,
};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
        BlimsCommand::Talk { agent_id } => start_blims_agent_talk(agent_id).await?,
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
    let mut app = BlimsTuiApp::load().await?;
    if should_show_world_picker().await? {
        app.show_world_picker = true;
    }
    let stdout = std::io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let mut terminal = Terminal::new(
        guard
            .writer_mut()
            .ok_or_else(|| std::io::Error::other("terminal guard writer unavailable"))?,
        blims_terminal_area()?,
    );
    terminal.draw(|frame| app.render(frame))?;
    loop {
        if let Some(event) = read_event()? {
            if handle_blims_tui_event(&mut app, event).await? {
                break;
            }
            terminal.resize(blims_terminal_area()?);
            terminal.draw(|frame| app.render(frame))?;
        }
    }
    let _stdout = guard.leave()?;
    Ok(())
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
            if matches!(stroke.key, KeyCode::Escape | KeyCode::Char('q')) {
                return Ok(true);
            }
            if handle_blims_tui_picker_key(app, stroke.key).await? {
                return Ok(false);
            }
            match stroke.key {
                KeyCode::Char('?') => app.show_help = !app.show_help,
                KeyCode::Char('w') => app.show_world_picker = true,
                KeyCode::Char('r') => app.report = load_blims_report().await?,
                KeyCode::Char('h') | KeyCode::Left | KeyCode::Up => app.move_previous().await?,
                KeyCode::Char('l') | KeyCode::Right | KeyCode::Down => app.move_next().await?,
                _ => {}
            }
        }
    }
    Ok(false)
}

async fn handle_blims_tui_picker_key(
    app: &mut BlimsTuiApp,
    key: KeyCode,
) -> Result<bool, CliError> {
    if !app.show_world_picker {
        return Ok(false);
    }
    match key {
        KeyCode::Up => app.select_previous_template(),
        KeyCode::Down => app.select_next_template(),
        KeyCode::Enter => app.activate_selected_template().await?,
        _ => return Ok(false),
    }
    Ok(true)
}

#[derive(Debug)]
struct BlimsTuiApp {
    world: BlimsWorldSnapshot,
    report: BlimsMorningReport,
    interactions: BlimsAvailableInteractions,
    templates: Vec<BlimsWorldTemplateSummary>,
    selected_template: usize,
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
        Ok(Self {
            world,
            report,
            interactions,
            templates,
            selected_template,
            show_world_picker: false,
            show_help: false,
            status: "Welcome CEO — choose a world with w, move with arrows/h/l.".to_string(),
        })
    }

    async fn refresh(&mut self) -> Result<(), CliError> {
        self.world = load_blims_world().await?;
        self.report = load_blims_report().await?;
        self.interactions = load_blims_interactions().await?;
        Ok(())
    }

    async fn move_next(&mut self) -> Result<(), CliError> {
        let next = next_room_id(&self.world, &self.interactions.room_id);
        self.world = move_blims_player(&next).await?;
        self.interactions = load_blims_interactions().await?;
        self.status = format!("Moved to {}", self.current_room_name());
        Ok(())
    }

    async fn move_previous(&mut self) -> Result<(), CliError> {
        let previous = previous_room_id(&self.world, &self.interactions.room_id);
        self.world = move_blims_player(&previous).await?;
        self.interactions = load_blims_interactions().await?;
        self.status = format!("Moved to {}", self.current_room_name());
        Ok(())
    }

    async fn activate_selected_template(&mut self) -> Result<(), CliError> {
        if !self.show_world_picker {
            return Ok(());
        }
        if let Some(template) = self.templates.get(self.selected_template) {
            select_world_template(template.id.clone(), true).await?;
            self.show_world_picker = false;
            self.status = format!("Selected {}", template.name);
            self.refresh().await?;
        }
        Ok(())
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
    }
}

async fn load_blims_world() -> Result<BlimsWorldSnapshot, CliError> {
    let response = call_blims_service("world.snapshot", blims_workspace_payload()?).await?;
    decode_blims_response::<BlimsWorldSnapshot>(response)
}

async fn load_blims_interactions() -> Result<BlimsAvailableInteractions, CliError> {
    let response =
        call_blims_service("world.available_interactions", blims_workspace_payload()?).await?;
    decode_blims_response::<BlimsAvailableInteractions>(response)
}

async fn move_blims_player(room_id: &str) -> Result<BlimsWorldSnapshot, CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "room_id": room_id,
    });
    let response = call_blims_service("world.move_player", serde_json::to_vec(&request)?).await?;
    decode_blims_response::<BlimsWorldSnapshot>(response)
}

async fn load_blims_report() -> Result<BlimsMorningReport, CliError> {
    let response = call_blims_service("report.morning", blims_workspace_payload()?).await?;
    decode_blims_response::<BlimsMorningReport>(response)
}

async fn load_world_templates() -> Result<Vec<BlimsWorldTemplateSummary>, CliError> {
    let response = call_blims_service("world.template_list", blims_workspace_payload()?).await?;
    decode_blims_response::<Vec<BlimsWorldTemplateSummary>>(response)
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
            " ←/→ ",
            Style::new()
                .fg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("move  "),
        Span::styled(
            "w",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" world picker  "),
        Span::styled(
            "r",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" refresh report  "),
        Span::styled(
            "?",
            Style::new()
                .fg(Color::BrightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" help  "),
        Span::styled(
            "q",
            Style::new()
                .fg(Color::BrightRed)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" quit  —  "),
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
        .title(" Office world ")
        .background(Style::new().bg(Color::Rgb(8, 18, 24)));
    panel.render(area, frame);
    let inner = panel.inner_area(area).inset(Insets::all(1));
    render_room_links(app, inner, frame);
    for room in &app.world.rooms {
        render_room_tile(app, room, inner, frame);
    }
}

fn render_room_links(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let points = app
        .world
        .rooms
        .iter()
        .map(|room| room_point(room, area))
        .collect::<Vec<_>>();
    for window in points.windows(2) {
        let [from, to] = window else { continue };
        let min_x = from.x.min(to.x);
        let max_x = from.x.max(to.x);
        let min_y = from.y.min(to.y);
        let max_y = from.y.max(to.y);
        for x in min_x..=max_x {
            frame.buffer_mut().set_cell(
                Point::new(x, from.y),
                "─",
                Style::new()
                    .fg(Color::BrightBlack)
                    .bg(Color::Rgb(8, 18, 24)),
            );
        }
        for y in min_y..=max_y {
            frame.buffer_mut().set_cell(
                Point::new(to.x, y),
                "│",
                Style::new()
                    .fg(Color::BrightBlack)
                    .bg(Color::Rgb(8, 18, 24)),
            );
        }
    }
}

fn render_room_tile(
    app: &BlimsTuiApp,
    room: &BlimsRoomSnapshot,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let point = room_point(room, area);
    let tile =
        Rect::new(point.x.saturating_sub(7), point.y.saturating_sub(2), 15, 5).intersection(area);
    let active = room.id == app.interactions.room_id;
    let bg = if active {
        Color::Rgb(52, 36, 74)
    } else {
        Color::Rgb(22, 28, 38)
    };
    let fg = if active {
        Color::BrightYellow
    } else {
        color_from_name(&room.color)
    };
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(fg).bg(bg)))
        .background(Style::new().bg(bg));
    panel.render(tile, frame);
    let inner = panel.inner_area(tile);
    let occupants = app
        .world
        .agents
        .iter()
        .filter(|agent| agent.room_id == room.id)
        .collect::<Vec<_>>();
    let marker = if active { "@" } else { room.symbol.as_str() };
    frame.write_line_with_fallback_style(
        inner,
        &Line::from_spans(vec![
            Span::styled(
                marker.to_string(),
                Style::new()
                    .fg(Color::BrightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                room.name.clone(),
                Style::new()
                    .fg(Color::BrightWhite)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Style::new().bg(bg),
    );
    if inner.height > 1 {
        frame.write_line_with_fallback_style(
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
            &Line::raw(format!(
                "{} +{}",
                room.room_kind, room.productivity_modifier
            )),
            Style::new().fg(Color::BrightBlack).bg(bg),
        );
    }
    if inner.height > 2 && !occupants.is_empty() {
        let names = occupants
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        frame.write_line_with_fallback_style(
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
            &Line::from_spans(vec![Span::styled(
                names,
                Style::new().fg(Color::BrightGreen),
            )]),
            Style::new().bg(bg),
        );
    }
}

fn room_point(room: &BlimsRoomSnapshot, area: Rect) -> Point {
    let max_world_x = 40_i64;
    let max_world_y = 14_i64;
    let x_scale = i64::from(area.width.saturating_sub(4)).max(1);
    let y_scale = i64::from(area.height.saturating_sub(4)).max(1);
    let x = i64::from(area.x) + 2 + room.x.saturating_mul(x_scale) / max_world_x;
    let y = i64::from(area.y) + 2 + room.y.saturating_mul(y_scale) / max_world_y;
    Point::new(clamp_i64_to_u16(x), clamp_i64_to_u16(y))
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
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(8),
        ],
    );
    if rows.len() != 3 {
        return;
    }
    render_current_room_panel(app, rows[0], frame);
    render_report_panel(app, rows[1], frame);
    render_interactions_panel(app, rows[2], frame);
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

fn render_report_panel(app: &BlimsTuiApp, area: Rect, frame: &mut Frame<'_>) {
    let mut lines = vec![Line::from_spans(vec![Span::styled(
        app.report.title.clone(),
        Style::new()
            .fg(Color::BrightCyan)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.extend(
        app.report
            .bullets
            .iter()
            .take(6)
            .map(|bullet| Line::raw(format!("• {bullet}"))),
    );
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
    lines.extend(
        app.interactions
            .interactions
            .iter()
            .take(5)
            .map(|interaction| {
                Line::from_spans(vec![
                    Span::styled(
                        format!("{} ", interaction.source),
                        Style::new().fg(Color::BrightBlack),
                    ),
                    Span::raw(interaction.label.clone()),
                ])
            }),
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

fn render_blims_help_modal(area: Rect, frame: &mut Frame<'_>) {
    let modal = centered(area, Size::new(70, 10));
    let panel = Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::BrightBlue)))
        .title(" Blims controls ")
        .background(Style::new().bg(Color::Rgb(18, 20, 38)));
    panel.render(modal, frame);
    let text = Text::from_lines(vec![
        Line::raw("←/h and →/l move between rooms"),
        Line::raw("w opens starter office picker"),
        Line::raw("r refreshes the morning report"),
        Line::raw("? toggles this help"),
        Line::raw("q exits the office"),
        Line::raw("Use CLI commands for AI chat/work while this TUI grows."),
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

fn clamp_i64_to_u16(value: i64) -> u16 {
    u16::try_from(value.clamp(0, i64::from(u16::MAX))).unwrap_or_default()
}
async fn start_blims_agent_talk(agent_id: String) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "agent_id": agent_id,
    });
    let response = call_blims_service("agent.talk_prompt", serde_json::to_vec(&request)?).await?;
    let prompt = decode_blims_response::<BlimsAgentTalkPrompt>(response)?;
    start_agent_talk_session(prompt).await
}

async fn start_agent_talk_session(prompt: BlimsAgentTalkPrompt) -> Result<(), CliError> {
    let session = BcodeClient::default_endpoint()
        .create_session(Some(format!("Blims talk: {}", prompt.agent_id)))
        .await?;
    BcodeClient::default_endpoint()
        .send_user_message(session.id, prompt.prompt.clone())
        .await?;
    record_blims_conversation(&prompt, &session.id).await?;
    println!("AI chat session with {}: {}", prompt.agent_id, session.id);
    println!("Attaching now. Press Ctrl-C to return to the Blims office.");
    attach_session(session.id).await?;
    Ok(())
}

async fn record_blims_conversation(
    prompt: &BlimsAgentTalkPrompt,
    session_id: &SessionId,
) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "conversation_id": prompt.conversation_id,
        "agent_id": prompt.agent_id,
        "session_id": session_id.to_string(),
        "summary": "Bcode conversation session opened from Blims office.",
    });
    call_blims_service("agent.record_conversation", serde_json::to_vec(&request)?).await?;
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

async fn select_world_template(template_id: String, json: bool) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "template_id": template_id,
    });
    let response =
        call_blims_service("world.select_template", serde_json::to_vec(&request)?).await?;
    if json {
        print_blims_service_response(response);
    } else {
        let world = decode_blims_response::<BlimsWorldSnapshot>(response)?;
        println!("selected world: {} ({})", world.theme, world.template_id);
        print_blims_world(&world);
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
