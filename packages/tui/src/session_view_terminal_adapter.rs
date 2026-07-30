//! Exclusive renderer-neutral transcript to terminal-document adaptation boundary.

use std::collections::{BTreeMap, BTreeSet};

use bcode_session_view_models::{TranscriptViewDocument, TranscriptViewItemId, ViewRevision};

use crate::transcript::terminal_item_from_shared;

use super::{TranscriptDocument, TranscriptDocumentDamage};

/// Adapts canonical `SessionView` transcript items into terminal-native items.
///
/// This is the only production module allowed to mutate `TranscriptDocument`.
#[derive(Debug, Clone, Default)]
pub struct SessionViewTerminalAdapter {
    document_revision: Option<ViewRevision>,
    item_revisions: BTreeMap<TranscriptViewItemId, ViewRevision>,
}

impl SessionViewTerminalAdapter {
    pub fn reset(&mut self) {
        self.document_revision = None;
        self.item_revisions.clear();
    }

    pub fn apply(
        &mut self,
        source: &TranscriptViewDocument,
        target: &mut TranscriptDocument,
    ) -> TranscriptDocumentDamage {
        if !target.source_index_is_consistent() {
            target
                .replace_from_shared(source.items.iter().map(terminal_item_from_shared).collect());
            self.item_revisions = source
                .items
                .iter()
                .map(|item| (item.id.clone(), item.revision))
                .collect();
            self.document_revision = Some(source.revision);
            return TranscriptDocumentDamage::FullReset;
        }
        let source_items = source.items.iter();
        let ordered_source_ids = source_items
            .clone()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let source_ids = ordered_source_ids.iter().cloned().collect::<BTreeSet<_>>();
        let previous_ids = self.item_revisions.keys().cloned().collect::<BTreeSet<_>>();
        let structural = source_ids != previous_ids;
        let mut changed = BTreeSet::new();
        target.retain_from_shared(&source_ids);
        self.item_revisions.retain(|id, _| source_ids.contains(id));
        for item in source_items {
            let target_has_item = target.source_index(&item.id).is_some();
            if !target_has_item || self.item_revisions.get(&item.id) != Some(&item.revision) {
                target.upsert_from_shared(terminal_item_from_shared(item));
                self.item_revisions.insert(item.id.clone(), item.revision);
                changed.insert(item.id.clone());
            }
        }
        target.reorder_from_shared(&ordered_source_ids);
        self.document_revision = Some(source.revision);
        if structural {
            TranscriptDocumentDamage::Structural
        } else if changed.is_empty() {
            TranscriptDocumentDamage::None
        } else {
            TranscriptDocumentDamage::Items(changed)
        }
    }
}
