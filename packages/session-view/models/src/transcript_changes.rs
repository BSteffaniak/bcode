//! In-process, id-only transcript change notifications.
//!
//! [`TranscriptChanges`] tells renderers in the same process which transcript items an
//! operation inserted, removed or revised, so they can update derived layout incrementally.
//! It carries no item payloads, no schema version, and no terminal, viewport, frame or damage
//! concepts, and it is never serialized: [`crate::SessionViewPatch`] remains the only portable
//! change contract.

use crate::{TranscriptViewItemId, ViewRevision};

/// Item-level changes produced by one transcript operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptChanges {
    /// Document revision before the operation.
    pub base_revision: ViewRevision,
    /// Document revision after the operation.
    pub revision: ViewRevision,
    /// Changes in application order.
    pub changes: Vec<TranscriptChange>,
}

impl TranscriptChanges {
    /// Create an empty change set between two revisions.
    #[must_use]
    pub const fn empty(base_revision: ViewRevision, revision: ViewRevision) -> Self {
        Self {
            base_revision,
            revision,
            changes: Vec::new(),
        }
    }

    /// Whether the change set contains no changes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Whether the change set changes item membership (insertion, removal or reset).
    #[must_use]
    pub fn is_structural(&self) -> bool {
        self.changes
            .iter()
            .any(|change| !matches!(change, TranscriptChange::Revised { .. }))
    }
}

/// One item-level transcript change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptChange {
    /// Items inserted in document order directly after `after` (`None`: at the start).
    Inserted {
        /// Item the inserted run follows, or `None` when inserted at the document start.
        after: Option<TranscriptViewItemId>,
        /// Inserted item ids in document order.
        ids: Vec<TranscriptViewItemId>,
    },
    /// Items removed from the document.
    Removed {
        /// Removed item ids.
        ids: Vec<TranscriptViewItemId>,
    },
    /// An existing item changed content at a new revision.
    Revised {
        /// Revised item id.
        id: TranscriptViewItemId,
        /// The item's new revision.
        revision: ViewRevision,
    },
    /// The whole document was replaced; consumers rebuild derived state.
    Reset,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_classification_ignores_revisions_only() {
        let id = TranscriptViewItemId::new("item:1");
        let mut changes = TranscriptChanges::empty(1, 2);
        assert!(changes.is_empty());
        changes.changes.push(TranscriptChange::Revised {
            id: id.clone(),
            revision: 2,
        });
        assert!(!changes.is_structural());
        changes
            .changes
            .push(TranscriptChange::Removed { ids: vec![id] });
        assert!(changes.is_structural());
    }
}
