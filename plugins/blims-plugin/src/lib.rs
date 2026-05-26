#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Blims AI company simulator plugin.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};
use switchy_database::query::SortDirection;
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
    /// State initialization worker panicked.
    #[error("Blims state initialization worker panicked")]
    WorkerPanicked,
    /// Required state was missing.
    #[error("Blims company state has not been created at {0}")]
    StateMissing(PathBuf),
    /// Persisted state row was missing an expected column.
    #[error("Blims state row is missing column {0}")]
    MissingColumn(&'static str),
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
    .map_err(|_| BlimsStateError::WorkerPanicked)??;

    Ok(company_status(working_directory))
}

async fn initialize_schema(database: &dyn Database) -> Result<(), switchy_database::DatabaseError> {
    create_core_tables(database).await?;
    seed_core_rows(database).await
}

async fn create_core_tables(
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
        .await?;
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
        .await?;
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
}

fn load_company_data(working_directory: &Path) -> Result<CompanyData, BlimsStateError> {
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
            load_company_data_from_database(database.as_ref()).await
        })
    })
    .join()
    .map_err(|_| BlimsStateError::WorkerPanicked)?
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

fn required_text(row: &Row, column: &'static str) -> Result<String, BlimsStateError> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToString::to_string))
        .ok_or(BlimsStateError::MissingColumn(column))
}

fn morning_report(data: &CompanyData) -> MorningReport {
    MorningReport {
        title: format!("{} morning report", data.company.name),
        bullets: vec![
            format!("Mission: {}", data.company.mission),
            format!("Culture: {}", data.company.culture),
            format!(
                "{} starter agents are active across {} rooms.",
                data.agents.len(),
                data.rooms.len()
            ),
            "No active initiatives yet. Add CEO guidance to wake the company loop.".to_string(),
        ],
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
