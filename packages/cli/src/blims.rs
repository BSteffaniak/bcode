#![allow(clippy::module_name_repetitions)]

use super::{CliError, attach_session, ensure_server_running, print_service_response};
use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bcode_worktree_models::{WorktreeBaseRef, WorktreeCreateRequest, WorktreeCreateResponse};
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
    Agents {
        #[arg(long)]
        json: bool,
    },
    World {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsAgentSnapshot {
    id: String,
    name: String,
    role: String,
    status: String,
    room_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsRoomSnapshot {
    id: String,
    name: String,
    purpose: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsWorldSnapshot {
    theme: String,
    player_name: String,
    rooms: Vec<BlimsRoomSnapshot>,
    agents: Vec<BlimsAgentSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsWorldInteraction {
    id: String,
    label: String,
    command: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsArtifactDetail {
    id: String,
    initiative_id: String,
    kind: String,
    title: String,
    payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BlimsAgentTalkPrompt {
    agent_id: String,
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
        BlimsCommand::Enter => enter_blims_office().await?,
        BlimsCommand::Talk { agent_id } => start_blims_agent_talk(agent_id).await?,
        BlimsCommand::Task { command } => handle_blims_task_command(command).await?,
        BlimsCommand::Artifact { command } => handle_blims_artifact_command(command).await?,
        BlimsCommand::Proposal { command } => handle_blims_proposal_command(command).await?,
        BlimsCommand::Initiative { command } => handle_blims_initiative_command(command).await?,
        BlimsCommand::Guidance { command } => handle_blims_guidance_command(command).await?,
        BlimsCommand::Report { json } => {
            let response = call_blims_service("report.morning", blims_workspace_payload()?).await?;
            if json {
                print_blims_service_response(response);
            } else {
                let report = decode_blims_response::<BlimsMorningReport>(response)?;
                println!("{}", report.title);
                for bullet in report.bullets {
                    println!("* {bullet}");
                }
            }
        }
    }
    Ok(())
}

async fn enter_blims_office() -> Result<(), CliError> {
    let mut world = load_blims_world().await?;
    let mut report = load_blims_report().await?;
    let mut interactions = load_blims_interactions().await?;
    let mut player_room_id = interactions.room_id.clone();
    loop {
        print_blims_office(&world, &report, &interactions, &player_room_id);
        print!("blims> ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let command = input.trim();
        if matches!(command, "q" | "quit" | "exit") {
            break;
        }
        match command {
            "h" | "left" => {
                player_room_id = previous_room_id(&world, &player_room_id);
                world = move_blims_player(&player_room_id).await?;
                interactions = load_blims_interactions().await?;
            }
            "l" | "right" | "j" | "down" | "k" | "up" => {
                player_room_id = next_room_id(&world, &player_room_id);
                world = move_blims_player(&player_room_id).await?;
                interactions = load_blims_interactions().await?;
            }
            "r" | "report" => print_blims_report(&report),
            "refresh" | "reload" => {
                refresh_blims_office(&mut world, &mut report).await?;
                interactions = load_blims_interactions().await?;
                player_room_id = interactions.room_id.clone();
            }
            "tasks" | "task" => print_office_tasks().await?,
            "artifacts" | "artifact" => print_office_artifacts().await?,
            "proposals" | "proposal" => print_office_proposals().await?,
            "initiatives" | "initiative" => print_office_initiatives().await?,
            "t" | "talk" | "look" => print_room_interaction(&world, &report, &player_room_id),
            "ai" => {
                start_room_agent_talk(&world, &player_room_id).await?;
                refresh_blims_office(&mut world, &mut report).await?;
            }
            "w" | "world" => print_blims_world(&world),
            "help" | "?" => print_blims_help(),
            "" => {}
            _ => handle_blims_office_command(command).await?,
        }
    }
    Ok(())
}

async fn refresh_blims_office(
    world: &mut BlimsWorldSnapshot,
    report: &mut BlimsMorningReport,
) -> Result<(), CliError> {
    *world = load_blims_world().await?;
    *report = load_blims_report().await?;
    println!("office refreshed");
    Ok(())
}

async fn handle_blims_office_command(command: &str) -> Result<(), CliError> {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["new" | "create", "initiative", rest @ ..] => {
            create_office_initiative(&rest.join(" ")).await?;
        }
        ["plan", "initiative", initiative_id] | ["plan", initiative_id] => {
            start_blims_initiative_plan((*initiative_id).to_string()).await?;
        }
        ["import", "plan", initiative_id, plan_path] => {
            import_blims_initiative_plan(
                (*initiative_id).to_string(),
                (*plan_path).to_string(),
                true,
            )
            .await?;
        }
        ["inspect", "initiative", initiative_id] | ["initiative", initiative_id] => {
            print_office_initiative(initiative_id).await?;
        }
        ["inspect", "task", task_id] | ["task", task_id] => {
            print_office_task(task_id).await?;
        }
        ["inspect", "artifact", artifact_id] | ["artifact", artifact_id] => {
            print_office_artifact(artifact_id).await?;
        }
        ["inspect", "proposal", proposal_id] | ["proposal", proposal_id] => {
            print_office_proposal(proposal_id).await?;
        }
        ["ready", proposal_id] | ["ready", "proposal", proposal_id] => {
            update_office_proposal_status(
                "proposal.mark_ready",
                "proposal ready for review",
                proposal_id,
            )
            .await?;
        }
        ["approve", proposal_id] | ["approve", "proposal", proposal_id] => {
            update_office_proposal_status("proposal.approve", "proposal approved", proposal_id)
                .await?;
        }
        ["reject", proposal_id] | ["reject", "proposal", proposal_id] => {
            update_office_proposal_status("proposal.reject", "proposal rejected", proposal_id)
                .await?;
        }
        ["defer", proposal_id] | ["defer", "proposal", proposal_id] => {
            update_office_proposal_status("proposal.defer", "proposal deferred", proposal_id)
                .await?;
        }
        ["patch", proposal_id] | ["patch", "proposal", proposal_id] => {
            create_blims_proposal_patch((*proposal_id).to_string(), false).await?;
        }
        ["apply", artifact_id] | ["apply", "artifact", artifact_id] => {
            apply_blims_patch_artifact((*artifact_id).to_string(), false).await?;
        }
        ["work", "task", task_id] | ["work", task_id] => {
            start_blims_task_work((*task_id).to_string()).await?;
        }
        ["talk" | "ai", agent_id] => {
            start_blims_agent_talk((*agent_id).to_string()).await?;
        }
        _ => println!("unknown command: {command} (try `help`)"),
    }
    Ok(())
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

async fn print_office_initiatives() -> Result<(), CliError> {
    let response = call_blims_service("initiative.list", blims_workspace_payload()?).await?;
    print_initiative_list(&decode_blims_response::<Vec<BlimsInitiativeSummary>>(
        response,
    )?);
    Ok(())
}

async fn create_office_initiative(title: &str) -> Result<(), CliError> {
    let title = title.trim();
    if title.is_empty() {
        println!("usage: new initiative <title>");
        return Ok(());
    }
    let request = BlimsInitiativeCreateRequest {
        working_directory: std::env::current_dir()?,
        title: title.to_string(),
        description: None,
        priority: None,
    };
    let response = call_blims_service("initiative.create", serde_json::to_vec(&request)?).await?;
    let initiative = decode_blims_response::<BlimsInitiativeSummary>(response)?;
    println!(
        "initiative created: {} ({})",
        initiative.title, initiative.id
    );
    println!(
        "Tip: run `plan {}` here in the office to ask Blims for a dynamic plan.",
        initiative.id
    );
    Ok(())
}

async fn print_office_initiative(initiative_id: &str) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "initiative_id": initiative_id,
    });
    let response = call_blims_service("initiative.inspect", serde_json::to_vec(&request)?).await?;
    print_initiative_detail(&decode_blims_response::<BlimsInitiativeSummary>(response)?);
    Ok(())
}

async fn print_office_tasks() -> Result<(), CliError> {
    let response = call_blims_service("task.list", blims_workspace_payload()?).await?;
    let tasks = decode_blims_response::<Vec<BlimsTaskSummary>>(response)?;
    if tasks.is_empty() {
        println!("no tasks yet");
    } else {
        for task in tasks {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                task.id,
                task.initiative_id,
                task.priority,
                task.status,
                task.assigned_agent_id,
                task.title
            );
        }
    }
    Ok(())
}

async fn print_office_task(task_id: &str) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "task_id": task_id,
    });
    let response = call_blims_service("task.inspect", serde_json::to_vec(&request)?).await?;
    print_task_detail(&decode_blims_response::<BlimsTaskSummary>(response)?);
    Ok(())
}

async fn print_office_artifacts() -> Result<(), CliError> {
    let response = call_blims_service("artifact.list", blims_workspace_payload()?).await?;
    let artifacts = decode_blims_response::<Vec<BlimsArtifactSummary>>(response)?;
    if artifacts.is_empty() {
        println!("no artifacts yet");
    } else {
        for artifact in artifacts {
            println!(
                "{}\t{}\t{}\t{}",
                artifact.id, artifact.initiative_id, artifact.kind, artifact.title
            );
        }
    }
    Ok(())
}

async fn print_office_artifact(artifact_id: &str) -> Result<(), CliError> {
    let request = serde_json::json!({
        "working_directory": std::env::current_dir()?,
        "artifact_id": artifact_id,
    });
    let response = call_blims_service("artifact.inspect", serde_json::to_vec(&request)?).await?;
    print_artifact_detail(&decode_blims_response::<BlimsArtifactDetail>(response)?);
    Ok(())
}

async fn print_office_proposals() -> Result<(), CliError> {
    let response = call_blims_service("proposal.list", blims_workspace_payload()?).await?;
    print_proposal_list(&decode_blims_response::<Vec<BlimsWorkProposalSummary>>(
        response,
    )?);
    Ok(())
}

async fn print_office_proposal(proposal_id: &str) -> Result<(), CliError> {
    let proposal = load_blims_proposal("proposal.inspect", proposal_id.to_string()).await?;
    print_proposal_detail(&proposal);
    Ok(())
}

async fn update_office_proposal_status(
    operation: &str,
    message: &str,
    proposal_id: &str,
) -> Result<(), CliError> {
    let proposal = load_blims_proposal(operation, proposal_id.to_string()).await?;
    println!("{message}: {}", proposal.id);
    print_proposal_detail(&proposal);
    Ok(())
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

async fn start_room_agent_talk(
    world: &BlimsWorldSnapshot,
    player_room_id: &str,
) -> Result<(), CliError> {
    let Some(agent) = world
        .agents
        .iter()
        .find(|agent| agent.room_id == player_room_id)
    else {
        println!("No agent is here to start an AI chat with.");
        return Ok(());
    };
    start_blims_agent_talk(agent.id.clone()).await
}

async fn start_agent_talk_session(prompt: BlimsAgentTalkPrompt) -> Result<(), CliError> {
    let session = BcodeClient::default_endpoint()
        .create_session(Some(format!("Blims talk: {}", prompt.agent_id)))
        .await?;
    BcodeClient::default_endpoint()
        .send_user_message(session.id, prompt.prompt)
        .await?;
    println!("AI chat session with {}: {}", prompt.agent_id, session.id);
    println!("Attaching now. Press Ctrl-C to return to the Blims office.");
    attach_session(session.id).await?;
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

fn print_blims_office(
    world: &BlimsWorldSnapshot,
    report: &BlimsMorningReport,
    interactions: &BlimsAvailableInteractions,
    player_room_id: &str,
) {
    print!("\x1B[2J\x1B[H");
    println!("┌────────────────────────────────────────────────────────────┐");
    println!("│ {:^58} │", world.theme);
    println!("├────────────────────────────────────────────────────────────┤");
    for room in &world.rooms {
        let marker = if room.id == player_room_id { "@" } else { " " };
        let occupants = world
            .agents
            .iter()
            .filter(|agent| agent.room_id == room.id)
            .map(|agent| agent.name.chars().next().unwrap_or('?'))
            .collect::<String>();
        println!(
            "│ {marker} {:<18} {:<22} {:>12} │",
            room.name, room.purpose, occupants
        );
    }
    println!("└────────────────────────────────────────────────────────────┘");
    println!();
    println!("@ {}", world.player_name);
    for agent in &world.agents {
        println!(
            "{} {} — {} — {}",
            agent.name.chars().next().unwrap_or('?'),
            agent.name,
            agent.role,
            agent.status
        );
    }
    println!();
    println!("{}", report.title);
    for bullet in report.bullets.iter().take(3) {
        println!("* {bullet}");
    }
    println!();
    println!("Available interactions:");
    for interaction in &interactions.interactions {
        println!("* {} — `{}`", interaction.label, interaction.command);
    }
    println!();
    print_blims_help();
}

fn print_blims_help() {
    println!(
        "Commands: h/left previous room, l/right next room, t talk/look/interactions, ai chat here, ai <agent>, r report, initiatives, tasks, artifacts, proposals, new initiative <title>, plan <initiative-id>, import plan <initiative-id> <file>, work <task-id>, ready/approve/reject/defer <proposal-id>, patch <proposal-id>, apply <artifact-id>, inspect <initiative|task|artifact|proposal> <id>, refresh, w world, q quit"
    );
}

fn print_room_interaction(
    world: &BlimsWorldSnapshot,
    report: &BlimsMorningReport,
    player_room_id: &str,
) {
    let Some(room) = world.rooms.iter().find(|room| room.id == player_room_id) else {
        println!("You are between rooms. The office hums softly.");
        return;
    };
    println!("You are in {} — {}", room.name, room.purpose);
    let agents = world
        .agents
        .iter()
        .filter(|agent| agent.room_id == room.id)
        .collect::<Vec<_>>();
    if agents.is_empty() {
        println!("No agents are here yet.");
        if room.id == "whiteboard" {
            println!("The whiteboard is ready for initiatives and CEO guidance.");
        }
        return;
    }
    for agent in agents {
        println!("{} says: {}", agent.name, agent_line(agent, report));
    }
}

fn agent_line(agent: &BlimsAgentSnapshot, report: &BlimsMorningReport) -> String {
    let context = report
        .bullets
        .iter()
        .find(|bullet| bullet.starts_with("Top initiative:") || bullet.starts_with("Top guidance:"))
        .cloned()
        .unwrap_or_else(|| "I'm waiting for the next CEO direction.".to_string());
    format!("I'm {}, {}. {}", agent.name, agent.status, context)
}

fn print_blims_report(report: &BlimsMorningReport) {
    println!("{}", report.title);
    for bullet in &report.bullets {
        println!("* {bullet}");
    }
}

fn print_blims_world(world: &BlimsWorldSnapshot) {
    println!("{}", world.theme);
    println!("player: {}", world.player_name);
    println!("rooms:");
    for room in &world.rooms {
        println!("* {} ({}) - {}", room.name, room.id, room.purpose);
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
                        "{}\t{}\t{}\t{}",
                        artifact.id, artifact.initiative_id, artifact.kind, artifact.title
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
