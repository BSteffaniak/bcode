#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Blims AI company simulator plugin.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};
use switchy_database::Database;
use thiserror::Error;

/// Blims plugin service interface id.
pub const BLIMS_SERVICE_INTERFACE_ID: &str = "bcode.blims/v1";

/// Company status operation.
pub const OP_COMPANY_STATUS: &str = "company.status";

/// Company create operation.
pub const OP_COMPANY_CREATE: &str = "company.create";

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
            OP_WORLD_SNAPSHOT => json_response(&world_snapshot()),
            OP_REPORT_MORNING => json_response(&morning_report()),
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

/// Snapshot of the currently visible Blims world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Starter office theme name.
    pub theme: String,
    /// Player avatar display name.
    pub player_name: String,
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
}

/// CEO morning report summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MorningReport {
    /// Report title.
    pub title: String,
    /// Report bullet items.
    pub bullets: Vec<String>,
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
    database
        .exec_raw(
            "CREATE TABLE IF NOT EXISTS blims_schema_version (\
             version INTEGER NOT NULL,\
             applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\
             )",
        )
        .await?;
    database
        .exec_raw(
            "CREATE TABLE IF NOT EXISTS companies (\
             id TEXT PRIMARY KEY NOT NULL,\
             name TEXT NOT NULL,\
             mission TEXT NOT NULL,\
             culture TEXT NOT NULL,\
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\
             )",
        )
        .await?;
    database
        .exec_raw(
            "CREATE TABLE IF NOT EXISTS events (\
             id INTEGER PRIMARY KEY AUTOINCREMENT,\
             company_id TEXT NOT NULL,\
             kind TEXT NOT NULL,\
             summary TEXT NOT NULL,\
             payload_json TEXT NOT NULL,\
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\
             )",
        )
        .await?;
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
        .await
}

fn world_snapshot() -> WorldSnapshot {
    WorldSnapshot {
        theme: "Cozy Startup Loft".to_string(),
        player_name: "CEO".to_string(),
        agents: vec![
            AgentSnapshot {
                id: "mira".to_string(),
                name: "Mira".to_string(),
                role: "Product Lead".to_string(),
                status: "waiting by the whiteboard".to_string(),
            },
            AgentSnapshot {
                id: "jules".to_string(),
                name: "Jules".to_string(),
                role: "Engineer".to_string(),
                status: "setting up a workbench".to_string(),
            },
            AgentSnapshot {
                id: "pip".to_string(),
                name: "Pip".to_string(),
                role: "Creative Generalist".to_string(),
                status: "sketching cozy office ideas".to_string(),
            },
        ],
    }
}

fn morning_report() -> MorningReport {
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
        let snapshot = world_snapshot();

        assert_eq!(snapshot.theme, "Cozy Startup Loft");
        assert_eq!(snapshot.agents.len(), 3);
    }

    fn tempfile_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bcode-blims-test-{name}-{}", std::process::id()))
    }
}
