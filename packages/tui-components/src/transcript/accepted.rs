//! Caller-owned semantic acceptance and dispatch for resident transcript items.

use super::item::{TranscriptItem, TranscriptItemKind};
use super::transcript_document::{SessionViewTerminalAdapter, TranscriptDocument};
use super::transcript_layout::{
    TranscriptLayoutCache, TranscriptLayoutFingerprint, TranscriptLayoutRows,
    TranscriptLayoutSignature, TranscriptLayoutSpec,
};
use super::transcript_markdown_cache::TranscriptMarkdownCache;
use crate::transcript_markdown::{MarkdownBlockStyle, append_markdown_block};
use bcode_markdown_render::MarkdownRenderOptions;
use bcode_session_view_models::{TextFormat, TranscriptViewDocument, TranscriptViewItemId};
use bmux_tui::prelude::{Line, Style};
use std::collections::{BTreeMap, BTreeSet};

/// Accepted rich adapter output. Rows, anchors and selection are replaced together.
/// Adapter execution is host-owned and must finish before composition.
#[derive(Debug, Clone)]
pub struct AcceptedVisual {
    /// Semantic revision this output was prepared from.
    pub item_revision: u64,
    /// Layout width, including transcript chrome.
    pub width: u16,
    /// Adapter-owned revision, for invalidation without a semantic event.
    pub revision: u64,
    /// Complete native output; never a patch to the previous snapshot.
    pub rows: TranscriptLayoutRows,
}

/// Resident presentation state, owned by the caller rather than recreated in paint.
#[derive(Debug, Clone, Default)]
pub struct AcceptedTranscript {
    /// Incrementally adapted semantic items, including plugin contributions.
    pub document: TranscriptDocument,
    /// The same indexed retained layout used by the native host.
    pub layout: TranscriptLayoutCache,
    /// One bounded set of projection variants per resident item.
    pub markdown: TranscriptMarkdownCache,
    adapter: SessionViewTerminalAdapter,
    visuals: BTreeMap<TranscriptViewItemId, AcceptedVisual>,
    visual_revision: u64,
    accepted_revision: Option<u64>,
    /// A rejected oversized resident window requires host paging, not truncation.
    pub window_rejected: bool,
    rejected_revision: Option<u64>,
}

impl AcceptedTranscript {
    /// Replace a complete plugin snapshot outside paint. Generation mismatch is
    /// rejected again during dispatch, so stale completions cannot replace rows.
    pub fn accept_visual(&mut self, id: TranscriptViewItemId, visual: AcceptedVisual) -> bool {
        if !visual_supported(&visual.rows) {
            return false;
        }
        let Some(index) = self.document.source_index(&id) else {
            return false;
        };
        let Some(item) = self.document.get(index) else {
            return false;
        };
        if item.source_view_item_revision() != Some(visual.item_revision) {
            return false;
        }
        if let Some(previous) = self.visuals.get(&id)
            && previous.item_revision == visual.item_revision
            && previous.width == visual.width
            && previous.revision >= visual.revision
        {
            return false;
        }
        self.visuals.insert(id, visual);
        self.visual_revision = self.visual_revision.wrapping_add(1);
        true
    }

    /// Resolve accepted producer source by stable identity and revision, never by
    /// presentation index or by a semantic item's fallback text.
    pub fn producer_source(
        &self,
        identity: &str,
        revision: u64,
        range: std::ops::Range<usize>,
    ) -> Option<String> {
        let mut segments = BTreeMap::new();
        for visual in self.visuals.values() {
            if let TranscriptLayoutRows::Selected { selection, .. } = &visual.rows {
                for row in selection
                    .values()
                    .filter(|row| row.identity == identity && row.revision == revision)
                {
                    segments.insert(row.byte_start, row.text.as_str());
                }
            }
        }
        let mut cursor = range.start;
        let mut result = String::new();
        for (start, text) in segments {
            let end = start.checked_add(text.len())?;
            if end <= cursor || start >= range.end {
                continue;
            }
            if start > cursor {
                return None;
            }
            let selected_end = end.min(range.end);
            result.push_str(text.get(cursor.checked_sub(start)?..selected_end - start)?);
            cursor = selected_end;
        }
        (cursor == range.end).then_some(result)
    }

    /// Accept a complete, already bounded resident semantic window and prepare its
    /// rows. Never invoke this method from `Component::paint`.
    pub fn prepare(&mut self, document: &TranscriptViewDocument, width: u16) {
        if self.accepted_revision == Some(document.revision)
            || self.rejected_revision == Some(document.revision)
        {
            self.prepare_layout(width);
            return;
        }
        if !resident_window_supported(document) {
            *self = Self::default();
            self.window_rejected = true;
            self.rejected_revision = Some(document.revision);
            self.document.push_ephemeral("resident-window".into(), TranscriptItem::new("System", "Resident transcript window exceeds the local projection budget; load a smaller history page.".into()));
            self.prepare_layout(width);
            return;
        }
        if self.window_rejected {
            *self = Self::default();
        }
        self.adapter
            .apply_document(document, &BTreeMap::new(), true, &mut self.document);
        self.accepted_revision = Some(document.revision);
        self.prepare_layout(width);
    }

    /// Accept stream integrity and reasoning visibility with the semantic snapshot.
    pub fn prepare_snapshot(
        &mut self,
        snapshot: &bcode_session_view_models::SessionViewSnapshot,
        width: u16,
    ) {
        if !resident_window_supported(&snapshot.transcript) {
            self.prepare(&snapshot.transcript, width);
            return;
        }
        if self.window_rejected {
            *self = Self::default();
        }
        self.adapter.apply(snapshot, &mut self.document);
        self.accepted_revision = Some(snapshot.transcript.revision);
        self.prepare_layout(width);
    }

    fn prepare_layout(&mut self, width: u16) {
        let fingerprint = TranscriptLayoutFingerprint::new(format!(
            "{}:{width}:{}",
            self.document.revision(),
            self.visual_revision
        ));
        if self.layout.is_current(&fingerprint) {
            return;
        }
        let ids: BTreeSet<_> = self
            .document
            .iter()
            .filter_map(|item| item.source_view_item_id().cloned())
            .collect();
        self.visuals.retain(|id, _| ids.contains(id));
        let items = self.document.items();
        self.markdown
            .retain_resident_iter(items.iter(), self.document.revision());
        let markdown = &self.markdown;
        let visuals = &self.visuals;
        let _ = self.layout.sync(TranscriptLayoutSpec {
            width,
            fingerprint,
            structural_fingerprint: TranscriptLayoutFingerprint::new(format!(
                "{}:{width}",
                items.len()
            )),
            transcript_len: items.len(),
            pending_len: 0,
            transcript_signature: |index| {
                let item = &items[index];
                TranscriptLayoutSignature::new(format!(
                    "{}:{}:{}",
                    item.id().get(),
                    item.revision(),
                    item.source_view_item_id()
                        .and_then(|id| visuals.get(id))
                        .map_or(0, |visual| visual.revision)
                ))
            },
            transcript_rows: |index| {
                let item = &items[index];
                if let Some(visual) = item.source_view_item_id().and_then(|id| visuals.get(id))
                    && visual.width == width
                    && item.source_view_item_revision() == Some(visual.item_revision)
                {
                    return visual.rows.clone();
                }
                let rows = semantic_rows(item, width, markdown);
                if rows.len() > usize::from(u16::MAX) {
                    TranscriptLayoutRows::Rendered(vec![Line::raw("Transcript item exceeds supported source geometry; load a smaller history page.")])
                } else { rows }
            },
            transcript_invocation_id: |index| {
                items[index].visual_invocation_id().map(str::to_owned)
            },
            pending_signature: |_| unreachable!("no pending entries"),
            pending_rows: |_| unreachable!("no pending entries"),
            history_banner_signature: || None,
            history_banner_rows: || TranscriptLayoutRows::Rendered(Vec::new()),
            reset: || false,
        });
        // Native drains these into telemetry; the browser must not accumulate a queue.
        let _ = self.layout.drain_sync_stats();
    }
}

/// Dispatch every supported native semantic family.
///
/// Rich visual rows supplied by adapters supersede this truthful semantic fallback;
/// opaque payloads are never printed as though they were human-readable content.
#[must_use]
pub fn semantic_rows(
    item: &TranscriptItem,
    width: u16,
    markdown: &TranscriptMarkdownCache,
) -> TranscriptLayoutRows {
    if matches!(
        item.kind(),
        TranscriptItemKind::ToolContribution {
            placement: bcode_session_models::ToolContributionPlacement::Hidden,
            ..
        }
    ) {
        return TranscriptLayoutRows::Rendered(Vec::new());
    }
    let mut rows = Vec::new();
    let title = item.display_role();
    let uses_markdown = item.text_format() == TextFormat::Markdown
        && matches!(
            item.kind(),
            TranscriptItemKind::UserMessage
                | TranscriptItemKind::AssistantMessage
                | TranscriptItemKind::System
                | TranscriptItemKind::ReasoningMessage
        );
    if uses_markdown {
        let options = MarkdownRenderOptions::new(width.saturating_sub(2))
            .with_document_id(format!("transcript:{}", item.id().get()))
            .with_streaming(item.streaming())
            .with_link_destination_fallbacks(true);
        let projection = markdown.project(item, options);
        let body_start = append_markdown_block(
            &mut rows,
            &title,
            &projection,
            width,
            MarkdownBlockStyle {
                heading: Style::new(),
                muted: Style::new(),
                prominent: matches!(item.kind(), TranscriptItemKind::UserMessage),
                body_indent: 2,
            },
        );
        return TranscriptLayoutRows::Markdown {
            rows,
            anchors: Vec::new(),
            projection,
            body_start,
        };
    }
    let heading = if item.streaming() {
        format!("{title} · streaming")
    } else {
        title
    };
    rows.extend(bmux_tui::text::wrap_line_bounded(
        &Line::raw(heading),
        bmux_tui::text::TextWrapGeometry::uniform(usize::from(width)),
        bmux_tui::text::TextWrap::Word,
    ));
    // The native adapter already produces semantic lifecycle, permission,
    // interaction, skill, draft and artifact fallback text for these families.
    for line in item.text().split('\n') {
        rows.extend(bmux_tui::text::wrap_line_bounded(
            &Line::raw(line),
            bmux_tui::text::TextWrapGeometry::uniform(usize::from(width)),
            bmux_tui::text::TextWrap::Word,
        ));
    }
    rows.push(Line::default());
    TranscriptLayoutRows::Rendered(rows)
}

#[cfg(test)]
mod tests;

/// Bound semantic adaptation before cloning or parsing. Larger history is paged by
/// the host; it is never silently parsed as a truncated complete document.
#[must_use]
pub fn resident_window_supported(document: &TranscriptViewDocument) -> bool {
    use bcode_session_models::retention::{ObservationRetentionLimits, ObservationRetentionUsage};
    if document.items.len() > 512 {
        return false;
    }
    let limits = ObservationRetentionLimits {
        scalar_capacity_bytes: 256 * 1024,
        collection_entries: 16_384,
        value_nodes: 32_768,
        depth: 32,
        copies: 1,
    };
    let mut usage = ObservationRetentionUsage::default();
    document.items.iter().all(|item| {
        bcode_session_view_models::retention::inspect_transcript_item(item, limits)
            .and_then(|item_usage| usage.try_add(item_usage, limits))
            .is_ok()
    })
}

fn visual_supported(bundle: &TranscriptLayoutRows) -> bool {
    let (rows, anchors, selection) = match bundle {
        TranscriptLayoutRows::Rendered(rows) => (rows, 0, None),
        TranscriptLayoutRows::Anchored { rows, anchors }
        | TranscriptLayoutRows::Markdown { rows, anchors, .. } => (rows, anchors.len(), None),
        TranscriptLayoutRows::Selected {
            rows,
            anchors,
            selection,
        } => (rows, anchors.len(), Some(selection)),
        TranscriptLayoutRows::BlankSpan(height) => return *height <= 8192,
    };
    if rows.len() > 8192 || anchors > 8192 {
        return false;
    }
    let bytes = rows
        .iter()
        .flat_map(|line| &line.spans)
        .try_fold(0usize, |sum, span| sum.checked_add(span.content.len()));
    if bytes.is_none_or(|bytes| bytes > 256 * 1024) {
        return false;
    }
    selection.is_none_or(|selection| {
        selection.len() <= rows.len()
            && selection
                .values()
                .all(bcode_plugin_models::tui_selection::PluginTuiSelectionRow::is_valid)
            && selection.values().map(|row| row.text.len()).sum::<usize>() <= 256 * 1024
    })
}
