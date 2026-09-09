//! Portable draft-edit application contracts.
//!
//! These types retain the established JSON representation used by daemon IPC. Edit compatibility
//! is recognized through `WorkflowAuthoringEditBatch::version`; unsupported batches are rejected
//! before mutation. Unknown request fields and unknown outcome variants are rejected, not guessed.

use serde::{Deserialize, Serialize};

use crate::{
    WorkflowAuthoringDocument, WorkflowAuthoringEditBatch, WorkflowDraftIdentity,
    WorkflowProducerProvenance, WorkflowValidationDiagnostic,
};

/// Portable mutable authored-workflow draft snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDraftSnapshot {
    /// Stable workflow and draft identity.
    pub identity: WorkflowDraftIdentity,
    /// Published revision from which this draft was derived, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<u64>,
    /// Current optimistic concurrency generation.
    pub generation: u64,
    /// Checksum of the authored document.
    pub checksum_sha256: String,
    /// Current authored content.
    pub document: WorkflowAuthoringDocument,
    /// Producer responsible for the current snapshot.
    pub producer: WorkflowProducerProvenance,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Last update time in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// Portable optimistic authoring conflict; no edit was applied by this request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringConflict {
    /// Identity of the conflicting entity.
    pub entity_id: String,
    /// Generation supplied by the caller.
    pub expected_generation: u64,
    /// Generation observed by the domain operation.
    pub current_generation: u64,
}

/// Typed result of an optimistic semantic draft edit batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDraftEditResult {
    /// Successfully committed snapshot.
    Updated(Box<WorkflowDraftSnapshot>),
    /// Generation conflict without mutation.
    Conflict(WorkflowAuthoringConflict),
    /// Semantic rejection without mutation.
    Rejected {
        /// Domain diagnostics explaining why the edits could not be applied.
        diagnostics: Vec<WorkflowValidationDiagnostic>,
    },
}

/// One generation-checked semantic draft edit request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyWorkflowDraftEditsRequest {
    /// Workflow owning the draft.
    pub workflow_id: String,
    /// Draft to edit.
    pub draft_id: String,
    /// Versioned ordered edits and exact expected generation.
    pub batch: WorkflowAuthoringEditBatch,
    /// Producer provenance evaluated by application authorization.
    pub producer: WorkflowProducerProvenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_edit_contract_preserves_wire_outcomes_and_rejects_future_variants() {
        let conflict = serde_json::json!({"conflict": {
            "entity_id": "draft", "expected_generation": 1, "current_generation": 2
        }});
        let result: WorkflowDraftEditResult = serde_json::from_value(conflict.clone()).unwrap();
        assert_eq!(serde_json::to_value(result).unwrap(), conflict);
        let rejected = serde_json::json!({"rejected": {"diagnostics": []}});
        let result: WorkflowDraftEditResult = serde_json::from_value(rejected.clone()).unwrap();
        assert_eq!(serde_json::to_value(result).unwrap(), rejected);
        assert!(
            serde_json::from_value::<WorkflowDraftEditResult>(
                serde_json::json!({"future_outcome": {}})
            )
            .is_err()
        );
    }
}
