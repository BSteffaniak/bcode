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

    /// Run producer-owned portable adapters outside BMUX paint. The same
    /// canonical visual envelope is used by native plugin dispatch.
    pub fn prepare_question_visuals(
        &mut self,
        theme: Option<bmux_tui_components::theme::ComponentTheme>,
    ) {
        use bcode_tui_components::transcript::{
            tool_visual::CanonicalToolVisual, transcript_layout::TranscriptLayoutRows,
        };
        self.accepted.prepare_visuals(|item, _width| {
            let CanonicalToolVisual::Plugin(visual) = CanonicalToolVisual::for_item(item, None)?;
            if visual.producer_plugin_id.as_deref() != Some("bcode.question")
                || visual.schema != "bcode.question.outcome"
                || visual.schema_version != 1
            {
                return None;
            }
            let outcome = serde_json::from_value::<bcode_question_plugin::QuestionToolOutcome>(
                visual.payload,
            )
            .ok()?;
            let rows = bcode_question_plugin::presentation::question_outcome_rows(
                &outcome,
                bcode_tui_components::tool_card::ToolCardStyle::from_component_theme(theme),
            );
            Some((
                visual.revision,
                TranscriptLayoutRows::Rendered(rows),
                Vec::new(),
            ))
        });
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
        self.accepted.copy(selection)
    }
}

/// Both hosts compose `session_ui::TranscriptChild`; this adapter preserves the
/// existing browser surface interface while the host owns viewport and editor.
pub struct TranscriptComponent<'a> {
    pub transcript: &'a Transcript,
    pub top: usize,
}
impl TranscriptComponent<'_> {
    const fn child(
        &self,
    ) -> bcode_tui_components::transcript::accepted::AcceptedTranscriptChild<'_> {
        self.transcript.accepted.child(
            "bcode-web/transcript",
            self.top,
            "bcode-web/transcript-selection",
        )
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
