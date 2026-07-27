//! Session catalog status, health, and entry models.

use crate::{SessionLoadStatusKind, actor, lease};
use bcode_session_models::SessionSummary;

/// Current asynchronous catalog discovery status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum CatalogLoadStatus {
    /// Catalog loading has not started.
    NotStarted,
    /// Catalog loading is in progress.
    Loading,
    /// Catalog loading completed.
    Loaded,
    /// Catalog loading failed with a diagnostic message.
    Failed(String),
}

/// First-class session health for normal runtime UX.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionHealth {
    /// DB-backed session is ready for normal runtime access.
    Ready,
    /// Session is inspectable but contains semantically opaque history and is read-only.
    DegradedReadOnly { issue_count: u64 },
    /// A known historical writer can be migrated by this build.
    Migratable { source: u64, target: u64 },
    /// A known historical writer is migratable but a live owner blocks exclusive maintenance.
    BlockedOwner {
        source: u64,
        target: u64,
        owners: Vec<lease::SessionLeaseOwner>,
    },
    /// Session storage requires a different writer epoch.
    WriterIncompatible { actual: Option<u64>, expected: u64 },
    /// A DB read model is missing or stale.
    ProjectionStale {
        projection: &'static str,
        checkpoint: Option<u64>,
        expected: u64,
    },
    /// Session storage exists but cannot be safely used without repair.
    RepairRequired { reason: String },
    /// No DB-backed session exists for the id.
    NotFound,
}

/// Native catalog entry with maintenance/access metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCatalogEntry {
    /// Bounded session summary.
    pub summary: SessionSummary,
    /// Whether the session runtime has loaded current state.
    pub load_status: SessionCatalogLoadStatus,
}

/// Session load status for catalog/status reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCatalogLoadStatus {
    /// Current runtime state is loaded.
    Current,
    /// Only bounded catalog metadata is loaded.
    SummaryOnly,
}

impl SessionCatalogEntry {
    pub(crate) fn from_snapshot(snapshot: actor::SessionSnapshot) -> Self {
        Self {
            summary: snapshot.summary,
            load_status: match snapshot.load_status {
                SessionLoadStatusKind::Current => SessionCatalogLoadStatus::Current,
                SessionLoadStatusKind::SummaryOnly => SessionCatalogLoadStatus::SummaryOnly,
            },
        }
    }
}
