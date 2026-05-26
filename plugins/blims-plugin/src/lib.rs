#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Blims AI company simulator plugin.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use switchy_database::query::{FilterableQuery as _, SortDirection, where_eq};
use switchy_database::schema::{Column, DataType, create_table};
use switchy_database::{Database, DatabaseValue, Row};
use thiserror::Error;

/// Blims plugin service interface id.
pub const BLIMS_SERVICE_INTERFACE_ID: &str = "bcode.blims/v1";

/// Company status operation.
pub const OP_COMPANY_STATUS: &str = "company.status";

/// Company create operation.
pub const OP_COMPANY_CREATE: &str = "company.create";

/// Agent list operation.
pub const OP_AGENT_LIST: &str = "agent.list";

/// Initiative create operation.
pub const OP_INITIATIVE_CREATE: &str = "initiative.create";

/// Initiative list operation.
pub const OP_INITIATIVE_LIST: &str = "initiative.list";

/// Guidance set operation.
pub const OP_GUIDANCE_SET: &str = "guidance.set";

/// Guidance list operation.
pub const OP_GUIDANCE_LIST: &str = "guidance.list";

/// Initiative planning prompt operation.
pub const OP_INITIATIVE_PLAN_PROMPT: &str = "initiative.plan_prompt";

/// Initiative plan import operation.
pub const OP_INITIATIVE_IMPORT_PLAN: &str = "initiative.import_plan";

/// Task list operation.
pub const OP_TASK_LIST: &str = "task.list";

/// Task inspect operation.
pub const OP_TASK_INSPECT: &str = "task.inspect";

/// Artifact list operation.
pub const OP_ARTIFACT_LIST: &str = "artifact.list";

/// Artifact inspect operation.
pub const OP_ARTIFACT_INSPECT: &str = "artifact.inspect";

/// Agent talk prompt operation.
pub const OP_AGENT_TALK_PROMPT: &str = "agent.talk_prompt";

/// World snapshot operation.
pub const OP_WORLD_SNAPSHOT: &str = "world.snapshot";

/// Morning report operation.
pub const OP_REPORT_MORNING: &str = "report.morning";

const MANIFEST: &str = include_str!("../bcode-plugin.toml");
const BLIMS_STATE_DIR_ENV: &str = "BCODE_BLIMS_STATE_DIR";
const DEFAULT_STATE_ROOT: &str = ".bcode/blims";
const DATABASE_FILE_NAME: &str = "blims.sqlite";
const SCHEMA_VERSION: i64 = 1;

/// Bundled Blims company simulator plugin.
#[derive(Default)]
pub struct BlimsPlugin;

impl RustPlugin for BlimsPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != BLIMS_SERVICE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported Blims service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_COMPANY_STATUS => service_company_status(&context.request),
            OP_COMPANY_CREATE => service_company_create(&context.request),
            OP_AGENT_LIST => service_agent_list(&context.request),
            OP_INITIATIVE_CREATE => service_initiative_create(&context.request),
            OP_INITIATIVE_LIST => service_initiative_list(&context.request),
            OP_GUIDANCE_SET => service_guidance_set(&context.request),
            OP_GUIDANCE_LIST => service_guidance_list(&context.request),
            OP_INITIATIVE_PLAN_PROMPT => service_initiative_plan_prompt(&context.request),
            OP_INITIATIVE_IMPORT_PLAN => service_initiative_import_plan(&context.request),
            OP_TASK_LIST => service_task_list(&context.request),
            OP_TASK_INSPECT => service_task_inspect(&context.request),
            OP_ARTIFACT_LIST => service_artifact_list(&context.request),
            OP_ARTIFACT_INSPECT => service_artifact_inspect(&context.request),
            OP_AGENT_TALK_PROMPT => service_agent_talk_prompt(&context.request),
            OP_WORLD_SNAPSHOT => service_world_snapshot(&context.request),
            OP_REPORT_MORNING => service_morning_report(&context.request),
            _ => ServiceResponse::error("unsupported_operation", "unsupported Blims operation"),
        }
    }
}

/// Request carrying the workspace root for repo-local Blims state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
}

/// Request to create a company initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeCreateRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative title.
    pub title: String,
    /// Optional initiative description.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional initiative priority. Lower numbers sort first.
    #[serde(default)]
    pub priority: Option<i64>,
}

/// Request to add CEO guidance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuidanceSetRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Guidance text.
    pub guidance: String,
    /// Guidance strength label.
    #[serde(default = "default_guidance_strength")]
    pub strength: String,
}

/// Request to build an AI planning prompt for an initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativePlanPromptRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative id to plan.
    pub initiative_id: String,
}

/// Request to build an AI talk prompt for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTalkPromptRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Blims agent id.
    pub agent_id: String,
}

/// Request to inspect a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInspectRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Task id.
    pub task_id: String,
}

/// Request to inspect an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInspectRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Artifact id.
    pub artifact_id: String,
}

/// Request to import an AI-generated plan for an initiative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeImportPlanRequest {
    /// Workspace or repository directory.
    pub working_directory: PathBuf,
    /// Initiative id receiving the plan.
    pub initiative_id: String,
    /// AI-generated plan payload.
    pub plan: AiInitiativePlan,
}

/// AI-generated initiative plan contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiInitiativePlan {
    /// Short plan summary.
    pub summary: String,
    /// Acceptance criteria proposed by AI.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Proposed tasks.
    #[serde(default)]
    pub tasks: Vec<AiTaskProposal>,
    /// Risks identified by AI.
    #[serde(default)]
    pub risks: Vec<String>,
    /// Questions for the CEO.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Cozy/fun game opportunities proposed by AI.
    #[serde(default)]
    pub cozy_game_opportunities: Vec<String>,
}

/// AI-generated task proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiTaskProposal {
    /// Task title.
    pub title: String,
    /// Task description.
    #[serde(default)]
    pub description: String,
    /// Suggested Blims agent owner.
    #[serde(default)]
    pub suggested_agent_id: Option<String>,
    /// AI rationale for this task.
    #[serde(default)]
    pub rationale: String,
    /// Task priority. Lower numbers sort first.
    #[serde(default = "default_task_priority")]
    pub priority: i64,
}

/// Planning prompt returned for Bcode AI/session orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativePlanningPrompt {
    /// Initiative id.
    pub initiative_id: String,
    /// Prompt text to send to an AI planning session.
    pub prompt: String,
}

/// Agent talk prompt returned for Bcode AI/session orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTalkPrompt {
    /// Agent id.
    pub agent_id: String,
    /// Prompt text to send to an AI conversation session.
    pub prompt: String,
}

/// Persisted task summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    /// Task id.
    pub id: String,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Task title.
    pub title: String,
    /// Task description.
    pub description: String,
    /// Task status.
    pub status: String,
    /// Assigned agent id, if any.
    pub assigned_agent_id: String,
    /// Task rationale.
    pub rationale: String,
    /// Task priority. Lower numbers sort first.
    pub priority: i64,
}

/// Persisted artifact summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    /// Artifact id.
    pub id: String,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Artifact kind.
    pub kind: String,
    /// Artifact title.
    pub title: String,
}

/// Persisted artifact detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDetail {
    /// Artifact id.
    pub id: String,
    /// Parent initiative id.
    pub initiative_id: String,
    /// Artifact kind.
    pub kind: String,
    /// Artifact title.
    pub title: String,
    /// Artifact payload JSON.
    pub payload_json: String,
}

/// Current Blims company lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanyLifecycleState {
    /// No repo-local Blims company has been created yet.
    NotStarted,
    /// Repo-local Blims company state exists.
    Created,
}

/// Blims company status summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanyStatus {
    /// Current company lifecycle state.
    pub state: CompanyLifecycleState,
    /// Human-readable status summary.
    pub message: String,
    /// Whether a Blims daemon is currently connected.
    pub daemon_connected: bool,
    /// Resolved Blims state root.
    pub state_root: PathBuf,
    /// Resolved Blims `SQLite` database path.
    pub database_path: PathBuf,
}

/// Blims world room snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSnapshot {
    /// Stable room identifier.
    pub id: String,
    /// Room display name.
    pub name: String,
    /// Room purpose or productivity modifier.
    pub purpose: String,
}

/// Snapshot of the currently visible Blims world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Starter office theme name.
    pub theme: String,
    /// Player avatar display name.
    pub player_name: String,
    /// Starter rooms currently available in the office.
    pub rooms: Vec<RoomSnapshot>,
    /// Starter agents currently visible in the office.
    pub agents: Vec<AgentSnapshot>,
}

/// Minimal visible agent state for the initial Blims office.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// Stable agent identifier.
    pub id: String,
    /// Agent display name.
    pub name: String,
    /// Current loose role or job title.
    pub role: String,
    /// Short current status.
    pub status: String,
    /// Current world room identifier.
    pub room_id: String,
}

/// CEO morning report summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MorningReport {
    /// Report title.
    pub title: String,
    /// Report bullet items.
    pub bullets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompanyRecord {
    name: String,
    mission: String,
    culture: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentRecord {
    id: String,
    name: String,
    role: String,
    department_id: String,
    team_id: String,
    status: String,
    room_id: String,
}

impl AgentRecord {
    fn snapshot(self) -> AgentSnapshot {
        AgentSnapshot {
            id: self.id,
            name: self.name,
            role: self.role,
            status: self.status,
            room_id: self.room_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoomRecord {
    id: String,
    name: String,
    purpose: String,
}

/// Persisted initiative summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeSummary {
    /// Initiative id.
    pub id: String,
    /// Initiative title.
    pub title: String,
    /// Initiative description.
    pub description: String,
    /// Initiative status.
    pub status: String,
    /// Initiative priority. Lower numbers sort first.
    pub priority: i64,
}

/// Persisted CEO guidance summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuidanceSummary {
    /// Guidance id.
    pub id: String,
    /// Guidance text.
    pub guidance: String,
    /// Guidance strength label.
    pub strength: String,
    /// Whether this guidance is active.
    pub active: bool,
}

fn default_guidance_strength() -> String {
    "strong".to_string()
}

const fn default_task_priority() -> i64 {
    100
}

/// Errors returned by Blims state initialization.
#[derive(Debug, Error)]
pub enum BlimsStateError {
    /// State directory creation failed.
    #[error("failed to create Blims state directory {path}: {source}")]
    CreateStateDirectory {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// `SQLite` initialization failed.
    #[error("failed to initialize Blims SQLite database {path}: {source}")]
    InitDatabase {
        /// Database path that could not be opened.
        path: PathBuf,
        /// Underlying database initialization error.
        source: switchy_database_connection::InitDbError,
    },
    /// Schema initialization failed.
    #[error("failed to initialize Blims database schema: {0}")]
    Schema(#[from] switchy_database::DatabaseError),
    /// JSON serialization failed.
    #[error("failed to encode Blims JSON payload: {0}")]
    Json(#[from] serde_json::Error),
    /// State initialization worker panicked.
    #[error("Blims state initialization worker panicked: {0}")]
    WorkerPanicked(String),
    /// Required state was missing.
    #[error("Blims company state has not been created at {0}")]
    StateMissing(PathBuf),
    /// Persisted state row was missing an expected column.
    #[error("Blims state row is missing column {0}")]
    MissingColumn(&'static str),
    /// A request field was invalid.
    #[error("invalid Blims request: {0}")]
    InvalidRequest(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatePaths {
    state_root: PathBuf,
    database_path: PathBuf,
}

#[must_use]
fn state_paths(working_directory: &Path) -> StatePaths {
    let state_root = env::var_os(BLIMS_STATE_DIR_ENV)
        .map_or_else(|| working_directory.join(DEFAULT_STATE_ROOT), PathBuf::from);
    let database_path = state_root.join(DATABASE_FILE_NAME);

    StatePaths {
        state_root,
        database_path,
    }
}

fn panic_to_blims_error(payload: Box<dyn Any + Send>) -> BlimsStateError {
    let message = payload.downcast::<String>().map_or_else(
        |payload| {
            payload.downcast::<&str>().map_or_else(
                |_| "unknown panic payload".to_string(),
                |message| (*message).to_string(),
            )
        },
        |message| *message,
    );
    BlimsStateError::WorkerPanicked(message)
}

fn service_company_status(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    json_response(&company_status(&request.working_directory))
}

fn service_company_create(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };

    match create_company_state(&request.working_directory) {
        Ok(status) => json_response(&status),
        Err(error) => ServiceResponse::error("state_initialization_failed", error.to_string()),
    }
}

fn service_agent_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };

    match load_company_data(&request.working_directory) {
        Ok(data) => json_response(
            &data
                .agents
                .into_iter()
                .map(AgentRecord::snapshot)
                .collect::<Vec<_>>(),
        ),
        Err(error) => ServiceResponse::error("state_read_failed", error.to_string()),
    }
}

fn service_initiative_create(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<InitiativeCreateRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match create_initiative(&request) {
        Ok(initiative) => json_response(&initiative),
        Err(error) => ServiceResponse::error("initiative_create_failed", error.to_string()),
    }
}

fn service_initiative_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_initiatives(&request.working_directory) {
        Ok(initiatives) => json_response(&initiatives),
        Err(error) => ServiceResponse::error("initiative_list_failed", error.to_string()),
    }
}

fn service_guidance_set(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<GuidanceSetRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match set_guidance(&request) {
        Ok(guidance) => json_response(&guidance),
        Err(error) => ServiceResponse::error("guidance_set_failed", error.to_string()),
    }
}

fn service_guidance_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_guidance(&request.working_directory) {
        Ok(guidance) => json_response(&guidance),
        Err(error) => ServiceResponse::error("guidance_list_failed", error.to_string()),
    }
}

fn service_initiative_plan_prompt(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<InitiativePlanPromptRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match build_initiative_plan_prompt(&request) {
        Ok(prompt) => json_response(&prompt),
        Err(error) => ServiceResponse::error("initiative_prompt_failed", error.to_string()),
    }
}

fn service_initiative_import_plan(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<InitiativeImportPlanRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match import_initiative_plan(&request) {
        Ok(plan) => json_response(&plan),
        Err(error) => ServiceResponse::error("initiative_import_failed", error.to_string()),
    }
}

fn service_task_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_tasks(&request.working_directory) {
        Ok(tasks) => json_response(&tasks),
        Err(error) => ServiceResponse::error("task_list_failed", error.to_string()),
    }
}

fn service_task_inspect(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<TaskInspectRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_task(&request) {
        Ok(task) => json_response(&task),
        Err(error) => ServiceResponse::error("task_inspect_failed", error.to_string()),
    }
}

fn service_artifact_list(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match list_artifacts(&request.working_directory) {
        Ok(artifacts) => json_response(&artifacts),
        Err(error) => ServiceResponse::error("artifact_list_failed", error.to_string()),
    }
}

fn service_artifact_inspect(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ArtifactInspectRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match inspect_artifact(&request) {
        Ok(artifact) => json_response(&artifact),
        Err(error) => ServiceResponse::error("artifact_inspect_failed", error.to_string()),
    }
}

fn service_agent_talk_prompt(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<AgentTalkPromptRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match build_agent_talk_prompt(&request) {
        Ok(prompt) => json_response(&prompt),
        Err(error) => ServiceResponse::error("agent_talk_prompt_failed", error.to_string()),
    }
}

fn service_world_snapshot(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<WorkspaceRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };

    match load_company_data(&request.working_directory) {
        Ok(data) => json_response(&WorldSnapshot {
            theme: data.world_theme,
            player_name: "CEO".to_string(),
            rooms: data
                .rooms
                .into_iter()
                .map(|room| RoomSnapshot {
                    id: room.id,
                    name: room.name,
                    purpose: room.purpose,
                })
                .collect(),
            agents: data
                .agents
                .into_iter()
                .map(|agent| AgentSnapshot {
                    id: agent.id,
                    name: agent.name,
                    role: agent.role,
                    status: agent.status,
                    room_id: agent.room_id,
                })
                .collect(),
        }),
        Err(_) => json_response(&fallback_world_snapshot()),
    }
}

fn service_morning_report(request: &ServiceRequest) -> ServiceResponse {
    let Ok(request) = request.payload_json::<WorkspaceRequest>() else {
        return json_response(&fallback_morning_report());
    };

    load_company_data(&request.working_directory).map_or_else(
        |_| json_response(&fallback_morning_report()),
        |data| json_response(&morning_report(&data)),
    )
}

fn company_status(working_directory: &Path) -> CompanyStatus {
    let paths = state_paths(working_directory);
    let state = if paths.database_path.exists() {
        CompanyLifecycleState::Created
    } else {
        CompanyLifecycleState::NotStarted
    };
    let message = match state {
        CompanyLifecycleState::Created => {
            "Blims company state exists. The office is ready to wake.".to_string()
        }
        CompanyLifecycleState::NotStarted => {
            "Blims is bundled and ready. Create a repo-local company to wake the office."
                .to_string()
        }
    };

    CompanyStatus {
        state,
        message,
        daemon_connected: false,
        state_root: paths.state_root,
        database_path: paths.database_path,
    }
}

fn create_company_state(working_directory: &Path) -> Result<CompanyStatus, BlimsStateError> {
    let paths = state_paths(working_directory);
    std::fs::create_dir_all(&paths.state_root).map_err(|source| {
        BlimsStateError::CreateStateDirectory {
            path: paths.state_root.clone(),
            source,
        }
    })?;

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime should build");
        runtime.block_on(async {
            let database = switchy_database_connection::init(Some(&paths.database_path), None)
                .await
                .map_err(|source| BlimsStateError::InitDatabase {
                    path: paths.database_path.clone(),
                    source,
                })?;
            initialize_schema(database.as_ref()).await?;
            Ok::<(), BlimsStateError>(())
        })
    })
    .join()
    .map_err(panic_to_blims_error)??;

    Ok(company_status(working_directory))
}

async fn initialize_schema(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    create_core_tables(database).await?;
    seed_core_rows(database).await
}

async fn create_core_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_base_tables(database).await?;
    create_org_tables(database).await?;
    create_world_tables(database).await?;
    create_work_tables(database).await
}

async fn create_base_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("blims_schema_version")
        .if_not_exists(true)
        .column(int_column("version"))
        .column(now_column("applied_at"))
        .execute(database)
        .await?;
    create_table("companies")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("name"))
        .column(text_column("mission"))
        .column(text_column("culture"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("events")
        .if_not_exists(true)
        .column(auto_id_column("id"))
        .column(text_column("company_id"))
        .column(text_column("kind"))
        .column(text_column("summary"))
        .column(text_column("payload_json"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await
}

async fn create_org_tables(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    create_table("departments")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("name"))
        .column(text_column("purpose"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("teams")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("department_id"))
        .column(text_column("name"))
        .column(text_column("purpose"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("agents")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("name"))
        .column(text_column("role"))
        .column(text_column("department_id"))
        .column(text_column("team_id"))
        .column(text_column("status"))
        .column(text_column("room_id"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("agent_contracts")
        .if_not_exists(true)
        .column(text_column("agent_id"))
        .column(text_column("responsibilities"))
        .column(text_column("restrictions"))
        .column(text_column("escalation"))
        .primary_key("agent_id")
        .execute(database)
        .await
}

async fn create_world_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("worlds")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("theme"))
        .column(text_column("player_room_id"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("world_rooms")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("world_id"))
        .column(text_column("name"))
        .column(text_column("purpose"))
        .primary_key("id")
        .execute(database)
        .await
}

async fn create_work_tables(
    database: &dyn Database,
) -> Result<(), switchy_database::DatabaseError> {
    create_table("executive_guidance")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("guidance"))
        .column(text_column("strength"))
        .column(bool_column("active"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("initiatives")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("company_id"))
        .column(text_column("title"))
        .column(text_column("description"))
        .column(text_column("status"))
        .column(int_column("priority"))
        .column(text_column("created_by"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("tasks")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("initiative_id"))
        .column(text_column("title"))
        .column(text_column("description"))
        .column(text_column("status"))
        .column(text_column("assigned_agent_id"))
        .column(text_column("rationale"))
        .column(int_column("priority"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await?;
    create_table("artifacts")
        .if_not_exists(true)
        .column(text_column("id"))
        .column(text_column("initiative_id"))
        .column(text_column("kind"))
        .column(text_column("title"))
        .column(text_column("payload_json"))
        .column(now_column("created_at"))
        .primary_key("id")
        .execute(database)
        .await
}

fn text_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

fn int_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn bool_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Bool,
        default: Some(DatabaseValue::Bool(true)),
    }
}

fn auto_id_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: true,
        data_type: DataType::BigInt,
        default: None,
    }
}

fn now_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::DateTime,
        default: Some(DatabaseValue::Now),
    }
}

async fn seed_core_rows(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    database
        .exec_raw(&format!(
            "INSERT INTO blims_schema_version (version) SELECT {SCHEMA_VERSION} \
             WHERE NOT EXISTS (SELECT 1 FROM blims_schema_version WHERE version = {SCHEMA_VERSION})"
        ))
        .await?;
    database
        .exec_raw(
            "INSERT INTO companies (id, name, mission, culture) \
             SELECT 'default', 'Blims', \
             'Build a cozy autonomous AI company inside Bcode.', \
             'cozy, fun, dynamic, productive' \
             WHERE NOT EXISTS (SELECT 1 FROM companies WHERE id = 'default')",
        )
        .await?;
    database
        .exec_raw(
            "INSERT INTO events (company_id, kind, summary, payload_json) \
             SELECT 'default', 'company_created', 'Blims company state initialized.', '{}' \
             WHERE NOT EXISTS (SELECT 1 FROM events WHERE kind = 'company_created')",
        )
        .await?;
    seed_departments(database).await?;
    seed_teams(database).await?;
    seed_world(database).await?;
    seed_agents(database).await
}

async fn seed_departments(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    for (id, name, purpose) in [
        (
            "product",
            "Product",
            "strategy, initiatives, and CEO reporting",
        ),
        (
            "engineering",
            "Engineering",
            "implementation, worktrees, tests, and review",
        ),
        (
            "creative",
            "Creative",
            "branding, docs tone, cozy design, and playful polish",
        ),
    ] {
        database
            .exec_raw(&format!(
                "INSERT INTO departments (id, company_id, name, purpose) \
                 SELECT '{id}', 'default', '{name}', '{purpose}' \
                 WHERE NOT EXISTS (SELECT 1 FROM departments WHERE id = '{id}')"
            ))
            .await?;
    }
    Ok(())
}

async fn seed_teams(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    for (id, department_id, name, purpose) in [
        (
            "product-leads",
            "product",
            "Product Leads",
            "turn CEO guidance into company direction",
        ),
        (
            "engineering-pod",
            "engineering",
            "Engineering Pod",
            "ship safe code in worktrees",
        ),
        (
            "creative-studio",
            "creative",
            "Creative Studio",
            "make Blims delightful and memorable",
        ),
    ] {
        database
            .exec_raw(&format!(
                "INSERT INTO teams (id, department_id, name, purpose) \
                 SELECT '{id}', '{department_id}', '{name}', '{purpose}' \
                 WHERE NOT EXISTS (SELECT 1 FROM teams WHERE id = '{id}')"
            ))
            .await?;
    }
    Ok(())
}

async fn seed_world(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    database
        .exec_raw(
            "INSERT INTO worlds (id, company_id, theme, player_room_id) \
             SELECT 'default', 'default', 'Cozy Startup Loft', 'ceo-nook' \
             WHERE NOT EXISTS (SELECT 1 FROM worlds WHERE id = 'default')",
        )
        .await?;
    for (id, name, purpose) in [
        (
            "ceo-nook",
            "CEO Nook",
            "company policy and executive guidance",
        ),
        (
            "whiteboard",
            "Whiteboard",
            "initiatives, priorities, and planning",
        ),
        (
            "engineering",
            "Engineering Desks",
            "implementation focus and worktree coding",
        ),
        (
            "creative",
            "Creative Corner",
            "branding, docs, and design ideas",
        ),
        ("review", "Review Wall", "artifact inspection and approval"),
    ] {
        database
            .exec_raw(&format!(
                "INSERT INTO world_rooms (id, world_id, name, purpose) \
                 SELECT '{id}', 'default', '{name}', '{purpose}' \
                 WHERE NOT EXISTS (SELECT 1 FROM world_rooms WHERE id = '{id}')"
            ))
            .await?;
    }
    Ok(())
}

async fn seed_agents(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    for agent in starter_agents() {
        database
            .exec_raw(&format!(
                "INSERT INTO agents (id, name, role, department_id, team_id, status, room_id) \
                 SELECT '{}', '{}', '{}', '{}', '{}', '{}', '{}' \
                 WHERE NOT EXISTS (SELECT 1 FROM agents WHERE id = '{}')",
                agent.id,
                agent.name,
                agent.role,
                agent.department_id,
                agent.team_id,
                agent.status,
                agent.room_id,
                agent.id
            ))
            .await?;
    }
    Ok(())
}

fn starter_agents() -> Vec<AgentRecord> {
    vec![
        AgentRecord {
            id: "mira".to_string(),
            name: "Mira".to_string(),
            role: "Product Lead".to_string(),
            department_id: "product".to_string(),
            team_id: "product-leads".to_string(),
            status: "waiting by the whiteboard".to_string(),
            room_id: "whiteboard".to_string(),
        },
        AgentRecord {
            id: "jules".to_string(),
            name: "Jules".to_string(),
            role: "Engineer".to_string(),
            department_id: "engineering".to_string(),
            team_id: "engineering-pod".to_string(),
            status: "setting up a workbench".to_string(),
            room_id: "engineering".to_string(),
        },
        AgentRecord {
            id: "pip".to_string(),
            name: "Pip".to_string(),
            role: "Creative Generalist".to_string(),
            department_id: "creative".to_string(),
            team_id: "creative-studio".to_string(),
            status: "sketching cozy office ideas".to_string(),
            room_id: "creative".to_string(),
        },
    ]
}

fn fallback_world_snapshot() -> WorldSnapshot {
    WorldSnapshot {
        theme: "Cozy Startup Loft".to_string(),
        player_name: "CEO".to_string(),
        rooms: vec![
            RoomSnapshot {
                id: "whiteboard".to_string(),
                name: "Whiteboard".to_string(),
                purpose: "initiatives, priorities, and planning".to_string(),
            },
            RoomSnapshot {
                id: "engineering".to_string(),
                name: "Engineering Desks".to_string(),
                purpose: "implementation focus and worktree coding".to_string(),
            },
            RoomSnapshot {
                id: "creative".to_string(),
                name: "Creative Corner".to_string(),
                purpose: "branding, docs, and design ideas".to_string(),
            },
        ],
        agents: starter_agents()
            .into_iter()
            .map(AgentRecord::snapshot)
            .collect(),
    }
}

fn fallback_morning_report() -> MorningReport {
    MorningReport {
        title: "Blims morning report".to_string(),
        bullets: vec![
            "The Blims plugin is now available as a bundled service stub.".to_string(),
            "Repo-local SQLite state initialization is available through company.create."
                .to_string(),
            "Starter office direction: Cozy Startup Loft, Hacker Garage, and Guild Hall."
                .to_string(),
        ],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompanyData {
    company: CompanyRecord,
    world_theme: String,
    rooms: Vec<RoomRecord>,
    agents: Vec<AgentRecord>,
    initiatives: Vec<InitiativeSummary>,
    guidance: Vec<GuidanceSummary>,
}

fn with_database<T>(
    working_directory: &Path,
    operation: impl for<'a> FnOnce(
        &'a dyn Database,
    )
        -> Pin<Box<dyn Future<Output = Result<T, BlimsStateError>> + 'a>>
    + Send
    + 'static,
) -> Result<T, BlimsStateError>
where
    T: Send + 'static,
{
    let paths = state_paths(working_directory);
    if !paths.database_path.exists() {
        return Err(BlimsStateError::StateMissing(paths.database_path));
    }

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime should build");
        runtime.block_on(async {
            let database = switchy_database_connection::init(Some(&paths.database_path), None)
                .await
                .map_err(|source| BlimsStateError::InitDatabase {
                    path: paths.database_path.clone(),
                    source,
                })?;
            initialize_schema(database.as_ref()).await?;
            operation(database.as_ref()).await
        })
    })
    .join()
    .map_err(panic_to_blims_error)?
}

fn load_company_data(working_directory: &Path) -> Result<CompanyData, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(load_company_data_from_database(database))
    })
}

fn create_initiative(
    request: &InitiativeCreateRequest,
) -> Result<InitiativeSummary, BlimsStateError> {
    let title = request.title.trim().to_string();
    if title.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "initiative title cannot be empty".to_string(),
        ));
    }
    let id = stable_slug(&title);
    let description = request.description.clone().unwrap_or_default();
    let priority = request.priority.unwrap_or(100);
    let initiative = InitiativeSummary {
        id: id.clone(),
        title: title.clone(),
        description: description.clone(),
        status: "active".to_string(),
        priority,
    };
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .insert("initiatives")
                .value("id", id)
                .value("company_id", "default")
                .value("title", title.clone())
                .value("description", description)
                .value("status", "active")
                .value("priority", priority)
                .value("created_by", "ceo")
                .execute(database)
                .await?;
            Ok::<_, BlimsStateError>(initiative)
        })
    })
}

fn list_initiatives(working_directory: &Path) -> Result<Vec<InitiativeSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("initiatives")
                .columns(&["id", "title", "description", "status", "priority"])
                .sort("priority", SortDirection::Asc)
                .execute(database)
                .await?
                .iter()
                .map(initiative_summary)
                .collect()
        })
    })
}

fn set_guidance(request: &GuidanceSetRequest) -> Result<GuidanceSummary, BlimsStateError> {
    let guidance = request.guidance.trim().to_string();
    if guidance.is_empty() {
        return Err(BlimsStateError::InvalidRequest(
            "guidance cannot be empty".to_string(),
        ));
    }
    let id = stable_slug(&guidance);
    let strength = request.strength.clone();
    let summary = GuidanceSummary {
        id: id.clone(),
        guidance: guidance.clone(),
        strength: strength.clone(),
        active: true,
    };
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .insert("executive_guidance")
                .value("id", id)
                .value("company_id", "default")
                .value("guidance", guidance.clone())
                .value("strength", strength)
                .value("active", true)
                .execute(database)
                .await?;
            Ok::<_, BlimsStateError>(summary)
        })
    })
}

fn list_guidance(working_directory: &Path) -> Result<Vec<GuidanceSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("executive_guidance")
                .columns(&["id", "guidance", "strength", "active"])
                .sort("created_at", SortDirection::Desc)
                .execute(database)
                .await?
                .iter()
                .map(guidance_summary)
                .collect()
        })
    })
}

fn list_tasks(working_directory: &Path) -> Result<Vec<TaskSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("tasks")
                .columns(&[
                    "id",
                    "initiative_id",
                    "title",
                    "description",
                    "status",
                    "assigned_agent_id",
                    "rationale",
                    "priority",
                ])
                .sort("priority", SortDirection::Asc)
                .execute(database)
                .await?
                .iter()
                .map(task_summary)
                .collect()
        })
    })
}

fn inspect_task(request: &TaskInspectRequest) -> Result<TaskSummary, BlimsStateError> {
    let task_id = request.task_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .select("tasks")
                .columns(&[
                    "id",
                    "initiative_id",
                    "title",
                    "description",
                    "status",
                    "assigned_agent_id",
                    "rationale",
                    "priority",
                ])
                .filter(Box::new(where_eq("id", task_id)))
                .limit(1)
                .execute_first(database)
                .await?
                .as_ref()
                .map(task_summary)
                .transpose()?
                .ok_or(BlimsStateError::MissingColumn("task"))
        })
    })
}

fn list_artifacts(working_directory: &Path) -> Result<Vec<ArtifactSummary>, BlimsStateError> {
    with_database(working_directory, |database| {
        Box::pin(async move {
            database
                .select("artifacts")
                .columns(&["id", "initiative_id", "kind", "title"])
                .sort("created_at", SortDirection::Desc)
                .execute(database)
                .await?
                .iter()
                .map(artifact_summary)
                .collect()
        })
    })
}

fn inspect_artifact(request: &ArtifactInspectRequest) -> Result<ArtifactDetail, BlimsStateError> {
    let artifact_id = request.artifact_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            database
                .select("artifacts")
                .columns(&["id", "initiative_id", "kind", "title", "payload_json"])
                .filter(Box::new(where_eq("id", artifact_id)))
                .limit(1)
                .execute_first(database)
                .await?
                .as_ref()
                .map(artifact_detail)
                .transpose()?
                .ok_or(BlimsStateError::MissingColumn("artifact"))
        })
    })
}

fn build_agent_talk_prompt(
    request: &AgentTalkPromptRequest,
) -> Result<AgentTalkPrompt, BlimsStateError> {
    let agent_id = request.agent_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let data = load_company_data_from_database(database).await?;
            let agent = data
                .agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .ok_or_else(|| {
                    BlimsStateError::InvalidRequest(format!("unknown Blims agent: {agent_id}"))
                })?;
            Ok(AgentTalkPrompt {
                agent_id,
                prompt: agent_talk_prompt_text(agent, &data),
            })
        })
    })
}

fn agent_talk_prompt_text(agent: &AgentRecord, data: &CompanyData) -> String {
    let initiatives = data
        .initiatives
        .iter()
        .map(|initiative| format!("* [{}] {}", initiative.status, initiative.title))
        .collect::<Vec<_>>()
        .join("\n");
    let guidance = data
        .guidance
        .iter()
        .map(|item| format!("* [{}] {}", item.strength, item.guidance))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are {}, a {} inside Blims, a cozy AI company simulator running inside Bcode.\n\n\
         Room: {}\nStatus: {}\nCompany mission: {}\nCulture: {}\n\n\
         Active CEO guidance:\n{}\n\nActive initiatives:\n{}\n\n\
         Reply in-character as this Blims agent. Be cozy, useful, dynamic, candid, and specific. \
         Tell the CEO what you are thinking about, what you recommend next, and whether anything needs attention.",
        agent.name,
        agent.role,
        agent.room_id,
        agent.status,
        data.company.mission,
        data.company.culture,
        if guidance.is_empty() {
            "* none"
        } else {
            &guidance
        },
        if initiatives.is_empty() {
            "* none"
        } else {
            &initiatives
        },
    )
}

fn build_initiative_plan_prompt(
    request: &InitiativePlanPromptRequest,
) -> Result<InitiativePlanningPrompt, BlimsStateError> {
    let initiative_id = request.initiative_id.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let data = load_company_data_from_database(database).await?;
            let initiative = load_initiative(database, &initiative_id).await?;
            Ok(InitiativePlanningPrompt {
                initiative_id,
                prompt: planning_prompt(&data, &initiative),
            })
        })
    })
}

fn import_initiative_plan(
    request: &InitiativeImportPlanRequest,
) -> Result<AiInitiativePlan, BlimsStateError> {
    let initiative_id = request.initiative_id.clone();
    let plan = request.plan.clone();
    let plan_for_response = plan.clone();
    with_database(&request.working_directory, move |database| {
        Box::pin(async move {
            let payload_json = serde_json::to_string(&plan)?;
            database
                .insert("artifacts")
                .value("id", format!("plan-{initiative_id}"))
                .value("initiative_id", initiative_id.clone())
                .value("kind", "ai_plan")
                .value("title", "AI-generated initiative plan")
                .value("payload_json", payload_json)
                .execute(database)
                .await?;
            for task in &plan.tasks {
                let task_id = format!("{}-{}", initiative_id, stable_slug(&task.title));
                database
                    .insert("tasks")
                    .value("id", task_id)
                    .value("initiative_id", initiative_id.clone())
                    .value("title", task.title.clone())
                    .value("description", task.description.clone())
                    .value("status", "proposed")
                    .value(
                        "assigned_agent_id",
                        task.suggested_agent_id.clone().unwrap_or_default(),
                    )
                    .value("rationale", task.rationale.clone())
                    .value("priority", task.priority)
                    .execute(database)
                    .await?;
            }
            Ok::<_, BlimsStateError>(plan_for_response)
        })
    })
}

async fn load_initiative(
    database: &dyn Database,
    initiative_id: &str,
) -> Result<InitiativeSummary, BlimsStateError> {
    database
        .select("initiatives")
        .columns(&["id", "title", "description", "status", "priority"])
        .filter(Box::new(where_eq("id", initiative_id)))
        .limit(1)
        .execute_first(database)
        .await?
        .as_ref()
        .map(initiative_summary)
        .transpose()?
        .ok_or(BlimsStateError::MissingColumn("initiative"))
}

fn planning_prompt(data: &CompanyData, initiative: &InitiativeSummary) -> String {
    let agents = data
        .agents
        .iter()
        .map(|agent| format!("* {} (`{}`): {}", agent.name, agent.id, agent.role))
        .collect::<Vec<_>>()
        .join("\n");
    let guidance = data
        .guidance
        .iter()
        .map(|item| format!("* [{}] {}", item.strength, item.guidance))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are Mira, the Product Lead inside Blims, a cozy AI company simulator for Bcode.\n\n\
         Company mission: {}\n\
         Company culture: {}\n\n\
         Active CEO guidance:\n{}\n\n\
         Current agents:\n{}\n\n\
         Initiative `{}`: {}\n\
         Description: {}\n\n\
         Create a dynamic, creative, useful initial plan. Return ONLY JSON matching this schema:\n\
         {{\"summary\": string, \"acceptance_criteria\": string[], \"tasks\": [{{\"title\": string, \"description\": string, \"suggested_agent_id\": string|null, \"rationale\": string, \"priority\": number}}], \"risks\": string[], \"questions\": string[], \"cozy_game_opportunities\": string[]}}",
        data.company.mission,
        data.company.culture,
        if guidance.is_empty() {
            "* none"
        } else {
            &guidance
        },
        agents,
        initiative.id,
        initiative.title,
        initiative.description,
    )
}

fn stable_slug(value: &str) -> String {
    value
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() || matches!(character, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

async fn load_company_data_from_database(
    database: &dyn Database,
) -> Result<CompanyData, BlimsStateError> {
    let company = database
        .select("companies")
        .columns(&["name", "mission", "culture"])
        .limit(1)
        .execute_first(database)
        .await?
        .ok_or(BlimsStateError::MissingColumn("companies"))?;
    let world = database
        .select("worlds")
        .columns(&["theme"])
        .limit(1)
        .execute_first(database)
        .await?
        .ok_or(BlimsStateError::MissingColumn("worlds"))?;
    let room_rows = database
        .select("world_rooms")
        .columns(&["id", "name", "purpose"])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?;
    let agent_rows = database
        .select("agents")
        .columns(&[
            "id",
            "name",
            "role",
            "department_id",
            "team_id",
            "status",
            "room_id",
        ])
        .sort("id", SortDirection::Asc)
        .execute(database)
        .await?;
    let initiatives = database
        .select("initiatives")
        .columns(&["id", "title", "description", "status", "priority"])
        .sort("priority", SortDirection::Asc)
        .execute(database)
        .await?
        .iter()
        .map(initiative_summary)
        .collect::<Result<Vec<_>, _>>()?;
    let guidance = database
        .select("executive_guidance")
        .columns(&["id", "guidance", "strength", "active"])
        .filter(Box::new(where_eq("active", true)))
        .sort("created_at", SortDirection::Desc)
        .execute(database)
        .await?
        .iter()
        .map(guidance_summary)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CompanyData {
        company: CompanyRecord {
            name: required_text(&company, "name")?,
            mission: required_text(&company, "mission")?,
            culture: required_text(&company, "culture")?,
        },
        world_theme: required_text(&world, "theme")?,
        rooms: room_rows
            .iter()
            .map(room_record)
            .collect::<Result<Vec<_>, _>>()?,
        agents: agent_rows
            .iter()
            .map(agent_record)
            .collect::<Result<Vec<_>, _>>()?,
        initiatives,
        guidance,
    })
}

fn room_record(row: &Row) -> Result<RoomRecord, BlimsStateError> {
    Ok(RoomRecord {
        id: required_text(row, "id")?,
        name: required_text(row, "name")?,
        purpose: required_text(row, "purpose")?,
    })
}

fn agent_record(row: &Row) -> Result<AgentRecord, BlimsStateError> {
    Ok(AgentRecord {
        id: required_text(row, "id")?,
        name: required_text(row, "name")?,
        role: required_text(row, "role")?,
        department_id: required_text(row, "department_id")?,
        team_id: required_text(row, "team_id")?,
        status: required_text(row, "status")?,
        room_id: required_text(row, "room_id")?,
    })
}

fn initiative_summary(row: &Row) -> Result<InitiativeSummary, BlimsStateError> {
    Ok(InitiativeSummary {
        id: required_text(row, "id")?,
        title: required_text(row, "title")?,
        description: required_text(row, "description")?,
        status: required_text(row, "status")?,
        priority: required_i64(row, "priority")?,
    })
}

fn guidance_summary(row: &Row) -> Result<GuidanceSummary, BlimsStateError> {
    Ok(GuidanceSummary {
        id: required_text(row, "id")?,
        guidance: required_text(row, "guidance")?,
        strength: required_text(row, "strength")?,
        active: required_bool(row, "active")?,
    })
}

fn task_summary(row: &Row) -> Result<TaskSummary, BlimsStateError> {
    Ok(TaskSummary {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        title: required_text(row, "title")?,
        description: required_text(row, "description")?,
        status: required_text(row, "status")?,
        assigned_agent_id: required_text(row, "assigned_agent_id")?,
        rationale: required_text(row, "rationale")?,
        priority: required_i64(row, "priority")?,
    })
}

fn artifact_summary(row: &Row) -> Result<ArtifactSummary, BlimsStateError> {
    Ok(ArtifactSummary {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        kind: required_text(row, "kind")?,
        title: required_text(row, "title")?,
    })
}

fn artifact_detail(row: &Row) -> Result<ArtifactDetail, BlimsStateError> {
    Ok(ArtifactDetail {
        id: required_text(row, "id")?,
        initiative_id: required_text(row, "initiative_id")?,
        kind: required_text(row, "kind")?,
        title: required_text(row, "title")?,
        payload_json: required_text(row, "payload_json")?,
    })
}

fn required_text(row: &Row, column: &'static str) -> Result<String, BlimsStateError> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToString::to_string))
        .ok_or(BlimsStateError::MissingColumn(column))
}

fn required_i64(row: &Row, column: &'static str) -> Result<i64, BlimsStateError> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or(BlimsStateError::MissingColumn(column))
}

fn required_bool(row: &Row, column: &'static str) -> Result<bool, BlimsStateError> {
    row.get(column)
        .and_then(|value| {
            value
                .as_bool()
                .or_else(|| value.as_i64().map(|value| value != 0))
        })
        .ok_or(BlimsStateError::MissingColumn(column))
}

fn morning_report(data: &CompanyData) -> MorningReport {
    let mut bullets = vec![
        format!("Mission: {}", data.company.mission),
        format!("Culture: {}", data.company.culture),
        format!(
            "{} starter agents are active across {} rooms.",
            data.agents.len(),
            data.rooms.len()
        ),
    ];
    if data.guidance.is_empty() {
        bullets.push("No active CEO guidance yet.".to_string());
    } else {
        bullets.push(format!("Active CEO guidance: {}", data.guidance.len()));
        if let Some(guidance) = data.guidance.first() {
            bullets.push(format!("Top guidance: {}", guidance.guidance));
        }
    }
    if data.initiatives.is_empty() {
        bullets.push(
            "No active initiatives yet. Add CEO guidance to wake the company loop.".to_string(),
        );
    } else {
        bullets.push(format!("Active initiatives: {}", data.initiatives.len()));
        if let Some(initiative) = data.initiatives.first() {
            bullets.push(format!("Top initiative: {}", initiative.title));
        }
    }

    MorningReport {
        title: format!("{} morning report", data.company.name),
        bullets,
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

export_plugin!(BlimsPlugin, MANIFEST);

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(BlimsPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn company_status_starts_not_started() {
        let temp = tempfile_path("status");
        let status = company_status(&temp);

        assert_eq!(status.state, CompanyLifecycleState::NotStarted);
        assert!(!status.daemon_connected);
        assert_eq!(status.state_root, temp.join(DEFAULT_STATE_ROOT));
    }

    #[test]
    fn state_paths_default_to_repo_local() {
        let cwd = PathBuf::from("/tmp/blims-repo");
        let paths = state_paths(&cwd);

        assert_eq!(paths.state_root, cwd.join(DEFAULT_STATE_ROOT));
        assert_eq!(
            paths.database_path,
            cwd.join(DEFAULT_STATE_ROOT).join(DATABASE_FILE_NAME)
        );
    }

    #[test]
    fn world_snapshot_has_starter_agents() {
        let snapshot = fallback_world_snapshot();

        assert_eq!(snapshot.theme, "Cozy Startup Loft");
        assert_eq!(snapshot.agents.len(), 3);
    }

    fn tempfile_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bcode-blims-test-{name}-{}", std::process::id()))
    }
}
