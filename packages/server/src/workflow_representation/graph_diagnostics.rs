//! Secret-safe categories for execution-scoped graph publication failures.

use bcode_workflow_store::representation::RepresentationError;

/// Normalized publication failure, without persistence payloads or inferred subcauses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PublicationFailure {
    /// The durable execution owner does not match the caller.
    #[error("workflow graph publication authority conflict")]
    Authority,
    /// Execution allowances prevent the operation.
    #[error("workflow graph publication capacity unavailable")]
    Capacity,
    /// The candidate does not satisfy admission requirements.
    #[error("workflow graph publication admission invalid")]
    InvalidAdmission,
    /// Durable state requires maintenance before publication.
    #[error("workflow graph publication requires maintenance")]
    Maintenance,
    /// The representation or lifecycle does not support the operation.
    #[error("workflow graph publication operation unsupported")]
    Unsupported,
    /// Persistence failed; whether effects committed is not inferred.
    #[error(
        "workflow graph publication storage unavailable; inspect retained state before retrying"
    )]
    StorageUnavailable,
}

impl PublicationFailure {
    /// Fixed public diagnostic code; never derived from an underlying error string.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Authority => "workflow_graph_publication_authority_conflict",
            Self::Capacity => "workflow_graph_publication_capacity_unavailable",
            Self::InvalidAdmission => "workflow_graph_publication_invalid_admission",
            Self::Maintenance => "workflow_graph_publication_maintenance_required",
            Self::Unsupported => "workflow_graph_publication_unsupported",
            Self::StorageUnavailable => "workflow_graph_publication_storage_unavailable",
        }
    }
}

pub(super) const fn normalize(error: &RepresentationError) -> crate::ServerError {
    let failure = match error {
        RepresentationError::AuthorityConflict => PublicationFailure::Authority,
        RepresentationError::CapacityUnavailable => PublicationFailure::Capacity,
        RepresentationError::InvalidAdmission | RepresentationError::InventoryStalePage => {
            PublicationFailure::InvalidAdmission
        }
        RepresentationError::MaintenanceRequired => PublicationFailure::Maintenance,
        RepresentationError::UnsupportedLifecycle | RepresentationError::UnsupportedStore => {
            PublicationFailure::Unsupported
        }
        RepresentationError::Database(switchy_database::DatabaseError::Busy) => {
            return crate::ServerError::WorkflowStorageBusy;
        }
        RepresentationError::Database(_) => PublicationFailure::StorageUnavailable,
    };
    crate::ServerError::WorkflowGraphPublicationFailure(failure)
}

#[cfg(test)]
#[path = "graph_diagnostics/tests.rs"]
mod tests;
