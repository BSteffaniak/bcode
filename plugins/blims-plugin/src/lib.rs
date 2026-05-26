#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Blims AI company simulator plugin.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// Blims plugin service interface id.
pub const BLIMS_SERVICE_INTERFACE_ID: &str = "bcode.blims/v1";

/// Company status operation.
pub const OP_COMPANY_STATUS: &str = "company.status";

/// World snapshot operation.
pub const OP_WORLD_SNAPSHOT: &str = "world.snapshot";

/// Morning report operation.
pub const OP_REPORT_MORNING: &str = "report.morning";

const MANIFEST: &str = include_str!("../bcode-plugin.toml");

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
            OP_COMPANY_STATUS => json_response(&company_status()),
            OP_WORLD_SNAPSHOT => json_response(&world_snapshot()),
            OP_REPORT_MORNING => json_response(&morning_report()),
            _ => ServiceResponse::error("unsupported_operation", "unsupported Blims operation"),
        }
    }
}

/// Current Blims company lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanyLifecycleState {
    /// No repo-local Blims company has been created yet.
    NotStarted,
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

fn company_status() -> CompanyStatus {
    CompanyStatus {
        state: CompanyLifecycleState::NotStarted,
        message: "Blims is bundled and ready. Create a repo-local company to wake the office."
            .to_string(),
        daemon_connected: false,
    }
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
            "No company has been created yet; the daemon and SQLite state are next.".to_string(),
            "Starter office direction: Cozy Startup Loft, Hacker Garage, and Guild Hall."
                .to_string(),
        ],
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
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
        let status = company_status();

        assert_eq!(status.state, CompanyLifecycleState::NotStarted);
        assert!(!status.daemon_connected);
    }

    #[test]
    fn world_snapshot_has_starter_agents() {
        let snapshot = world_snapshot();

        assert_eq!(snapshot.theme, "Cozy Startup Loft");
        assert_eq!(snapshot.agents.len(), 3);
    }
}
