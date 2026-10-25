//! Browser compatibility adapter over the shared accepted transcript engine.

use bcode_tui_components::transcript::accepted::AcceptedTranscript;
use bcode_tui_components::transcript::transcript_layout::VisibleTranscriptSource;
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutNode};
use bmux_tui::paint::PaintCx;

/// Caller-owned resident transcript state. No bounded-preview projection exists.
#[derive(Default)]
pub struct Transcript {
    pub accepted: AcceptedTranscript,
    pub rows: usize,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Preferred host integration: preserve integrity and reasoning display policy.
    pub fn prepare_snapshot(
        &mut self,
        snapshot: &bcode_session_view_models::SessionViewSnapshot,
        width: u16,
    ) {
        self.accepted.prepare_snapshot(snapshot, width);
        self.rows = self.accepted.layout.total_rows();
    }

    pub fn anchor(&self, row: usize) -> Option<(String, usize)> {
        let visible = self.accepted.layout.line_at_row(row)?;
        let item = self.accepted.document.get(visible.entry_index)?;
        let id = item.source_view_item_id()?.get().to_owned();
        Some((
            id,
            self.accepted
                .layout
                .source_position(visible.entry_index, visible.row_in_entry)
                .unwrap_or(0),
        ))
    }

    pub fn resolve_anchor(&self, anchor: &(String, usize)) -> Option<usize> {
        self.accepted
            .document
            .iter()
            .enumerate()
            .find_map(|(index, item)| {
                if item.source_view_item_id()?.get() != anchor.0 {
                    return None;
                }
                Some(
                    self.accepted
                        .layout
                        .entry_start_row(VisibleTranscriptSource::Transcript, index)?
                        .saturating_add(
                            self.accepted
                                .layout
                                .source_row(index, anchor.1)
                                .unwrap_or(0),
                        ),
                )
            })
    }

    pub fn copy(&self, selection: &bmux_tui::selection::SelectionSnapshot) -> Option<String> {
        let mut output = String::new();
        for slice in &selection.slices {
            if let Some(source) = self.accepted.producer_source(
                slice.content_id.as_str(),
                slice.revision,
                slice.source_range.clone(),
            ) {
                output.push_str(&source);
                continue;
            }
            let mut found = false;
            for item in self.accepted.document.iter() {
                let id = format!("transcript:{}", item.id().get());
                if slice
                    .content_id
                    .as_str()
                    .strip_prefix(&id)
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.'))
                {
                    if slice.revision != item.revision() {
                        return None;
                    }
                    let suffix = &slice.content_id.as_str()[id.len()..];
                    let ranges = if let Some(code_id) = suffix.strip_prefix('.') {
                        let projection = self
                            .accepted
                            .layout
                            .markdown(
                                self.accepted
                                    .document
                                    .iter()
                                    .position(|candidate| candidate.id() == item.id())?,
                            )?
                            .0;
                        bcode_tui_components::markdown_selection::expand_code_selection_range(
                            item.text(),
                            projection.code_block_selections(),
                            code_id,
                            slice.source_range.clone(),
                        )?
                    } else {
                        vec![slice.source_range.clone()]
                    };
                    for range in ranges {
                        output.push_str(item.text().get(range)?);
                    }
                    found = true;
                    break;
                }
            }
            if !found {
                return None;
            }
        }
        Some(output)
    }
}

/// Both hosts compose `session_ui::TranscriptChild`; this adapter preserves the
/// existing browser surface interface while the host owns viewport and editor.
pub struct TranscriptComponent<'a> {
    pub transcript: &'a Transcript,
    pub top: usize,
}
impl TranscriptComponent<'_> {
    fn child(&self) -> bcode_tui_components::session_ui::TranscriptChild<'_> {
        bcode_tui_components::session_ui::TranscriptChild {
            id: "bcode-web/transcript",
            layout: &self.transcript.accepted.layout,
            items: self.transcript.accepted.document.items(),
            top: self.top,
            selection_scope: "bcode-web/transcript-selection",
            metadata: true,
            decorate: None,
        }
    }
}
impl Component for TranscriptComponent<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.child().layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.child().paint(layout, cx);
    }
}
