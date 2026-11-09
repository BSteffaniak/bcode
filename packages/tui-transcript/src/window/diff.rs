//! Keyed id/revision diff of the transcript document (SD5).
//!
//! Until `bcode_session_view::window_ops` lands (phase 5), V2 derives [`TranscriptChanges`]
//! by comparing the document after an operation with the item ids and revisions it last saw.
//! The diff runs only when the document revision changed.

use std::collections::{BTreeMap, BTreeSet};

use bcode_session_view_models::{
    TranscriptChange, TranscriptChanges, TranscriptViewDocument, TranscriptViewItem,
    TranscriptViewItemId, ViewRevision,
};

/// The item ids and revisions of the last diffed document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentMirror {
    revision: Option<ViewRevision>,
    items: Vec<(TranscriptViewItemId, ViewRevision)>,
}

impl DocumentMirror {
    /// Create an empty mirror (nothing seen yet).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            revision: None,
            items: Vec::new(),
        }
    }

    /// Item ids and revisions last seen, in document order.
    #[must_use]
    pub fn items(&self) -> &[(TranscriptViewItemId, ViewRevision)] {
        &self.items
    }

    /// Diff the document against the mirror and update the mirror.
    ///
    /// `previous` optionally supplies the full previous items (taken before a rebuild); when
    /// given, an item whose revision is unchanged but whose content differs is still reported
    /// as `Revised`, because a rebuild may recreate an item with different content at the same
    /// revision number.
    pub fn diff(
        &mut self,
        document: &TranscriptViewDocument,
        previous: Option<&[TranscriptViewItem]>,
    ) -> TranscriptChanges {
        let base = self.revision.unwrap_or_default();
        if self.revision == Some(document.revision) && previous.is_none() {
            return TranscriptChanges::empty(base, document.revision);
        }
        let changes = keyed_changes(&self.items, &document.items, previous);
        self.revision = Some(document.revision);
        self.items = document
            .items
            .iter()
            .map(|item| (item.id.clone(), item.revision))
            .collect();
        TranscriptChanges {
            base_revision: base,
            revision: document.revision,
            changes,
        }
    }
}

fn keyed_changes(
    old: &[(TranscriptViewItemId, ViewRevision)],
    new: &[TranscriptViewItem],
    previous: Option<&[TranscriptViewItem]>,
) -> Vec<TranscriptChange> {
    let old_revisions = old
        .iter()
        .map(|(id, revision)| (id, *revision))
        .collect::<BTreeMap<_, _>>();
    let new_ids = new.iter().map(|item| &item.id).collect::<BTreeSet<_>>();
    let previous_items = previous.map(|items| {
        items
            .iter()
            .map(|item| (&item.id, item))
            .collect::<BTreeMap<_, _>>()
    });

    // Surviving ids must keep their relative order; otherwise consumers reset.
    let surviving_old = old
        .iter()
        .map(|(id, _)| id)
        .filter(|id| new_ids.contains(id));
    let surviving_new = new
        .iter()
        .map(|item| &item.id)
        .filter(|id| old_revisions.contains_key(id));
    if !surviving_old.eq(surviving_new) {
        return vec![TranscriptChange::Reset];
    }

    let mut changes = Vec::new();
    let removed = old
        .iter()
        .filter(|(id, _)| !new_ids.contains(id))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    if !removed.is_empty() {
        changes.push(TranscriptChange::Removed { ids: removed });
    }
    let mut previous_id: Option<&TranscriptViewItemId> = None;
    let mut run: Option<(Option<TranscriptViewItemId>, Vec<TranscriptViewItemId>)> = None;
    for item in new {
        match old_revisions.get(&item.id) {
            None => {
                run.get_or_insert_with(|| (previous_id.cloned(), Vec::new()))
                    .1
                    .push(item.id.clone());
            }
            Some(old_revision) => {
                if let Some((after, ids)) = run.take() {
                    changes.push(TranscriptChange::Inserted { after, ids });
                }
                let content_changed = previous_items
                    .as_ref()
                    .and_then(|items| items.get(&item.id))
                    .is_some_and(|before| *before != item);
                if *old_revision != item.revision || content_changed {
                    changes.push(TranscriptChange::Revised {
                        id: item.id.clone(),
                        revision: item.revision,
                    });
                }
            }
        }
        previous_id = Some(&item.id);
    }
    if let Some((after, ids)) = run.take() {
        changes.push(TranscriptChange::Inserted { after, ids });
    }
    changes
}

#[cfg(test)]
mod tests {
    use bcode_session_view_models::{ChatMessageView, TranscriptViewItemKind};

    use super::*;

    fn item(id: &str, revision: u64, text: &str) -> TranscriptViewItem {
        TranscriptViewItem {
            id: TranscriptViewItemId::new(id),
            revision,
            sequence: None,
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown(text),
            },
        }
    }

    fn document(revision: u64, items: Vec<TranscriptViewItem>) -> TranscriptViewDocument {
        TranscriptViewDocument {
            revision,
            items,
            ..TranscriptViewDocument::default()
        }
    }

    fn id(value: &str) -> TranscriptViewItemId {
        TranscriptViewItemId::new(value)
    }

    #[test]
    fn sd5_diff_reports_insertions_removals_and_revisions_by_key() {
        let mut mirror = DocumentMirror::new();
        let first = document(
            1,
            vec![item("a", 1, "a"), item("b", 1, "b"), item("c", 1, "c")],
        );
        let initial = mirror.diff(&first, None);
        assert_eq!(
            initial.changes,
            vec![TranscriptChange::Inserted {
                after: None,
                ids: vec![id("a"), id("b"), id("c")]
            }]
        );

        let second = document(
            2,
            vec![
                item("p", 1, "p"),
                item("a", 1, "a"),
                item("b", 2, "b2"),
                item("d", 1, "d"),
            ],
        );
        let changes = mirror.diff(&second, None);
        assert_eq!(changes.base_revision, 1);
        assert_eq!(changes.revision, 2);
        assert_eq!(
            changes.changes,
            vec![
                TranscriptChange::Removed { ids: vec![id("c")] },
                TranscriptChange::Inserted {
                    after: None,
                    ids: vec![id("p")]
                },
                TranscriptChange::Revised {
                    id: id("b"),
                    revision: 2
                },
                TranscriptChange::Inserted {
                    after: Some(id("b")),
                    ids: vec![id("d")]
                },
            ]
        );
    }

    #[test]
    fn sd5_diff_runs_only_when_the_document_revision_changed() {
        let mut mirror = DocumentMirror::new();
        let doc = document(3, vec![item("a", 1, "a")]);
        let _ = mirror.diff(&doc, None);
        assert!(mirror.diff(&doc, None).is_empty());
    }

    #[test]
    fn rebuild_content_change_at_same_revision_is_revised() {
        let mut mirror = DocumentMirror::new();
        let before = vec![item("a", 1, "old")];
        let _ = mirror.diff(&document(1, before.clone()), None);
        let after = document(2, vec![item("a", 1, "new")]);
        assert_eq!(
            mirror.diff(&after, Some(&before)).changes,
            vec![TranscriptChange::Revised {
                id: id("a"),
                revision: 1
            }]
        );
    }

    #[test]
    fn reorder_reports_reset() {
        let mut mirror = DocumentMirror::new();
        let _ = mirror.diff(
            &document(1, vec![item("a", 1, "a"), item("b", 1, "b")]),
            None,
        );
        let changes = mirror.diff(
            &document(2, vec![item("b", 1, "b"), item("a", 1, "a")]),
            None,
        );
        assert_eq!(changes.changes, vec![TranscriptChange::Reset]);
    }
}
