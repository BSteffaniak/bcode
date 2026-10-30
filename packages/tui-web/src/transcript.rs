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
    registry:
        Option<bcode_tui_components::transcript::tool_visual::portable::PortableVisualRegistry>,
    registry_generation: u64,
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

    /// Rebuild the immutable registry only when effective native policy changes.
    pub fn configure_visuals(
        &mut self,
        policy: &crate::policy::BrowserVisualPolicy,
    ) -> Result<(), &'static str> {
        use bcode_tui_components::transcript::tool_visual::portable::{
            PortableVisualPolicy, PortableVisualRegistry,
        };
        self.registry_generation = self.registry_generation.saturating_add(1);
        let mut registrations = bcode_question_plugin::portable_visual_registrations();
        registrations.extend(bcode_filesystem_plugin::portable_visual_registrations());
        registrations.extend(bcode_shell_plugin::portable_visual_registrations());
        self.registry = Some(PortableVisualRegistry::new(
            self.registry_generation,
            PortableVisualPolicy {
                enabled_plugins: policy.enabled_plugins.clone(),
                preferred: policy.preferred.clone(),
                disabled: policy.disabled.clone(),
            },
            registrations,
        )?);
        Ok(())
    }

    /// Prepare producer-owned implementations outside BMUX paint, with one
    /// atomic accepted geometry for rows, images, selections and narrow actions.
    pub fn prepare_visuals(&mut self, theme: Option<bmux_tui_components::theme::ComponentTheme>) {
        if let Some(registry) = &self.registry {
            self.accepted
                .prepare_portable_visuals(registry, theme.as_ref());
        }
        self.rows = self.accepted.layout.total_rows();
    }

    /// Translate only a current accepted producer action; the caller owns the
    /// committed-scene gate. No platform execution or geometry mutation occurs.
    pub fn translate_action(
        &self,
        provenance: &bcode_tui_components::transcript::accepted::AcceptedActionProvenance,
        theme: Option<&bmux_tui_components::theme::ComponentTheme>,
    ) -> Option<bcode_tui_components::transcript::actions::TranscriptAction> {
        use bcode_tui_components::transcript::tool_visual::{
            CanonicalToolVisual, portable::PortableVisualContext,
        };
        let item = self
            .accepted
            .document
            .get(self.accepted.document.source_index(&provenance.item_id)?)?;
        if item.source_view_item_revision() != Some(provenance.item_revision) {
            return None;
        }
        let CanonicalToolVisual::Plugin(visual) = CanonicalToolVisual::for_item(item, None)?;
        self.registry.as_ref()?.translate_action(
            &visual,
            &PortableVisualContext {
                width: provenance.width,
                theme,
            },
            &provenance.action_id,
        )
    }

    pub fn artifact_preview_limit(
        &self,
        item_id: &str,
        width: u16,
        reference_key: &str,
        content_type: Option<&str>,
    ) -> Option<u64> {
        use bcode_tui_components::transcript::tool_visual::{
            CanonicalToolVisual, portable::PortableVisualContext,
        };
        let id = bcode_session_view_models::TranscriptViewItemId::new(item_id);
        let item = self
            .accepted
            .document
            .get(self.accepted.document.source_index(&id)?)?;
        let CanonicalToolVisual::Plugin(visual) = CanonicalToolVisual::for_item(item, None)?;
        self.registry.as_ref()?.artifact_preview_limit(
            &visual,
            &PortableVisualContext { width, theme: None },
            reference_key,
            content_type,
        )
    }

    pub fn has_visual(&self, item_id: &str, width: u16) -> bool {
        use bcode_tui_components::transcript::tool_visual::{
            CanonicalToolVisual, portable::PortableVisualContext,
        };
        let Some(registry) = &self.registry else {
            return false;
        };
        let id = bcode_session_view_models::TranscriptViewItemId::new(item_id);
        let Some(item) = self
            .accepted
            .document
            .source_index(&id)
            .and_then(|index| self.accepted.document.get(index))
        else {
            return false;
        };
        let Some(CanonicalToolVisual::Plugin(visual)) = CanonicalToolVisual::for_item(item, None)
        else {
            return false;
        };
        registry
            .prepare(&visual, &PortableVisualContext { width, theme: None })
            .is_some()
    }

    pub fn deliver_artifact(
        &self,
        item_id: &str,
        width: u16,
        chunk: &bcode_tui_components::transcript::tool_visual::portable::PortableArtifactChunk<'_>,
    ) -> bool {
        use bcode_tui_components::transcript::tool_visual::{
            CanonicalToolVisual, portable::PortableVisualContext,
        };
        let Some(registry) = &self.registry else {
            return false;
        };
        let id = bcode_session_view_models::TranscriptViewItemId::new(item_id);
        let Some(item) = self
            .accepted
            .document
            .source_index(&id)
            .and_then(|index| self.accepted.document.get(index))
        else {
            return false;
        };
        let Some(CanonicalToolVisual::Plugin(visual)) = CanonicalToolVisual::for_item(item, None)
        else {
            return false;
        };
        let Some(prepared) =
            registry.prepare(&visual, &PortableVisualContext { width, theme: None })
        else {
            return false;
        };
        registry.deliver_artifact(prepared.adapter_reference(), chunk)
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
