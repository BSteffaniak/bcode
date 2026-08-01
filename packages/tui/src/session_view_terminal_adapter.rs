//! Exclusive renderer-neutral transcript to terminal-document adaptation boundary.

use std::collections::{BTreeMap, BTreeSet};

use bcode_session_view_models::{SessionViewSnapshot, TranscriptViewItemId, ViewRevision};

use crate::transcript::{
    TranscriptStreamIntegrity, apply_stream_integrity, terminal_item_from_shared,
};

use super::{TranscriptDocument, TranscriptDocumentDamage};

/// Adapts canonical `SessionView` transcript items into terminal-native items.
///
/// This is the only production module allowed to mutate `TranscriptDocument`.
#[derive(Debug, Clone, Default)]
pub struct SessionViewTerminalAdapter {
    document_revision: Option<ViewRevision>,
    item_revisions: BTreeMap<TranscriptViewItemId, ViewRevision>,
    stream_statuses:
        BTreeMap<TranscriptViewItemId, bcode_session_view_models::TextStreamViewStatus>,
}

const fn stream_integrity(
    status: Option<bcode_session_view_models::TextStreamViewStatus>,
) -> Option<TranscriptStreamIntegrity> {
    match status {
        Some(bcode_session_view_models::TextStreamViewStatus::Incomplete) => {
            Some(TranscriptStreamIntegrity::Incomplete)
        }
        Some(bcode_session_view_models::TextStreamViewStatus::Degraded) => {
            Some(TranscriptStreamIntegrity::Degraded)
        }
        Some(
            bcode_session_view_models::TextStreamViewStatus::Healthy
            | bcode_session_view_models::TextStreamViewStatus::Terminal(_),
        )
        | None => None,
    }
}

impl SessionViewTerminalAdapter {
    pub fn reset(&mut self) {
        self.document_revision = None;
        self.item_revisions.clear();
        self.stream_statuses.clear();
    }

    pub fn apply(
        &mut self,
        snapshot: &SessionViewSnapshot,
        target: &mut TranscriptDocument,
    ) -> TranscriptDocumentDamage {
        let source = &snapshot.transcript;
        let source_items = source.items.iter().filter(|item| {
            snapshot.thinking.visible
                || !matches!(
                    item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ReasoningMessage { .. }
                        | bcode_session_view_models::TranscriptViewItemKind::ReasoningActivity { .. }
                )
        });
        if !target.source_index_is_consistent() {
            target.replace_from_shared(
                source_items
                    .clone()
                    .map(|item| {
                        apply_stream_integrity(
                            terminal_item_from_shared(item),
                            snapshot.text_streams.get(&item.id),
                        )
                    })
                    .collect(),
            );
            self.item_revisions = source_items
                .clone()
                .map(|item| (item.id.clone(), item.revision))
                .collect();
            self.stream_statuses = snapshot
                .text_streams
                .iter()
                .filter(|(id, _)| source_items.clone().any(|item| item.id == **id))
                .map(|(id, state)| (id.clone(), state.status))
                .collect();
            self.document_revision = Some(source.revision);
            return TranscriptDocumentDamage::FullReset;
        }
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
        self.stream_statuses.retain(|id, _| source_ids.contains(id));
        for item in source_items {
            let target_has_item = target.source_index(&item.id).is_some();
            let stream_status = snapshot
                .text_streams
                .get(&item.id)
                .map(|state| state.status);
            if !target_has_item || self.item_revisions.get(&item.id) != Some(&item.revision) {
                target.upsert_from_shared(apply_stream_integrity(
                    terminal_item_from_shared(item),
                    snapshot.text_streams.get(&item.id),
                ));
                changed.insert(item.id.clone());
            } else if self.stream_statuses.get(&item.id).copied() != stream_status
                && target.set_stream_integrity(&item.id, stream_integrity(stream_status))
            {
                changed.insert(item.id.clone());
            }
            self.item_revisions.insert(item.id.clone(), item.revision);
            if let Some(status) = stream_status {
                self.stream_statuses.insert(item.id.clone(), status);
            } else {
                self.stream_statuses.remove(&item.id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::{
        ChatMessageView, TextStreamViewState, TranscriptViewItem, TranscriptViewItemKind,
    };

    fn assistant_snapshot(
        revision: u64,
        status: bcode_session_view_models::TextStreamViewStatus,
    ) -> SessionViewSnapshot {
        let mut snapshot = SessionViewSnapshot::empty();
        let id = TranscriptViewItemId::new("assistant-stream");
        snapshot.transcript.items.push(TranscriptViewItem {
            id: id.clone(),
            revision,
            sequence: None,
            timestamp_ms: None,
            output_location: None,
            streaming: true,
            kind: TranscriptViewItemKind::AssistantMessage {
                message: ChatMessageView::markdown("answer"),
            },
        });
        snapshot.transcript.revision = revision;
        snapshot.text_streams.insert(
            id,
            TextStreamViewState {
                generation: 0,
                revision,
                accepted_bytes: 6,
                truncated: matches!(
                    status,
                    bcode_session_view_models::TextStreamViewStatus::Incomplete
                ),
                status,
            },
        );
        snapshot
    }

    #[test]
    fn hidden_reasoning_is_removed_without_reinterpreting_shared_items() {
        let mut snapshot = SessionViewSnapshot::empty();
        snapshot.thinking.visible = true;
        snapshot.transcript.items = vec![
            TranscriptViewItem {
                id: TranscriptViewItemId::new("reasoning"),
                revision: 1,
                sequence: Some(1),
                timestamp_ms: None,
                output_location: None,
                streaming: false,
                kind: TranscriptViewItemKind::ReasoningMessage {
                    message: ChatMessageView::markdown("private reasoning"),
                },
            },
            TranscriptViewItem {
                id: TranscriptViewItemId::new("answer"),
                revision: 1,
                sequence: Some(2),
                timestamp_ms: None,
                output_location: None,
                streaming: false,
                kind: TranscriptViewItemKind::AssistantMessage {
                    message: ChatMessageView::markdown("answer"),
                },
            },
        ];
        snapshot.transcript.revision = 1;
        let mut adapter = SessionViewTerminalAdapter::default();
        let mut document = TranscriptDocument::default();

        adapter.apply(&snapshot, &mut document);
        assert_eq!(document.items().len(), 2);

        snapshot.thinking.visible = false;
        snapshot.transcript.revision = 2;
        assert_eq!(
            adapter.apply(&snapshot, &mut document),
            TranscriptDocumentDamage::Structural
        );
        assert_eq!(document.items().len(), 1);
        assert_eq!(document.items()[0].text(), "answer");

        snapshot.thinking.visible = true;
        snapshot.transcript.revision = 3;
        assert_eq!(
            adapter.apply(&snapshot, &mut document),
            TranscriptDocumentDamage::Structural
        );
        assert_eq!(document.items().len(), 2);
    }

    #[test]
    fn integrity_only_change_updates_one_terminal_item() {
        let mut adapter = SessionViewTerminalAdapter::default();
        let mut document = TranscriptDocument::default();
        let healthy =
            assistant_snapshot(1, bcode_session_view_models::TextStreamViewStatus::Healthy);
        assert_eq!(
            adapter.apply(&healthy, &mut document),
            TranscriptDocumentDamage::Structural
        );
        let item_revision = document.items()[0].revision();

        let degraded =
            assistant_snapshot(1, bcode_session_view_models::TextStreamViewStatus::Degraded);
        assert_eq!(
            adapter.apply(&degraded, &mut document),
            TranscriptDocumentDamage::Items(BTreeSet::from([TranscriptViewItemId::new(
                "assistant-stream",
            )]))
        );
        assert_eq!(document.items()[0].text(), "answer");
        assert_eq!(
            document.items()[0].stream_integrity(),
            Some(TranscriptStreamIntegrity::Degraded)
        );
        assert!(document.items()[0].revision() > item_revision);
    }
}
