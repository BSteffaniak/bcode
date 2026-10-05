//! Availability of reduced mixed-family catalogs, independently of rich query presentation.
use crate::WorkflowCatalogFamilyHealth;
use serde::{Deserialize, Serialize};

/// Mixed pages are ordered by exact run identity; requested rich controls are not applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBasicCatalogAvailability {
    pub legacy: WorkflowCatalogFamilyHealth,
    pub version2: WorkflowCatalogFamilyHealth,
    /// False means requested filter/sort/search/group were not applied.
    pub requested_query_applied: bool,
    /// Rows expose canonical lifecycle only; zero counters mean unobserved, not measured totals.
    pub progress_available: bool,
    /// Identities whose summary cannot be authoritative; no fabricated lifecycle row is returned.
    pub unavailable_run_ids: Vec<String>,
}
