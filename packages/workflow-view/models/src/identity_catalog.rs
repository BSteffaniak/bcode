//! Bounded cross-representation identity discovery, distinct from rich filtered catalogs.
use serde::{Deserialize, Serialize};

/// Version of the independently evolving identity catalog contract.
pub const WORKFLOW_IDENTITY_CATALOG_VERSION: u32 = 1;

/// Run-ID ordered query. New arrivals behind the cursor require a fresh scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowIdentityCatalogRequest {
    pub version: u32,
    pub after: Option<String>,
    pub limit: u16,
}

/// Observed representation-family health, not execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCatalogFamilyHealth {
    Available,
    Absent,
    Unsupported,
    Degraded,
}

/// Membership never grants authority; Conflict selects neither canonical root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCatalogMembership {
    Legacy,
    Version2,
    Conflict,
}

/// One exact discovered run identity, without invented status or progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCatalogIdentity {
    pub run_id: String,
    pub membership: WorkflowCatalogMembership,
}

/// Bounded identity page. Exhausted cursor is NOT complete discovery if a family is degraded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowIdentityCatalogPage {
    pub version: u32,
    pub entries: Vec<WorkflowCatalogIdentity>,
    pub next_after: Option<String>,
    pub legacy: WorkflowCatalogFamilyHealth,
    pub version2: WorkflowCatalogFamilyHealth,
}
