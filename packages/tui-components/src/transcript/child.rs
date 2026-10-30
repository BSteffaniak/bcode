//! A retained transcript child. Preparation owns parsing and adapter work; paint
//! only reads the exact rows and source geometry accepted by the caller.

use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::selection::{SelectionFragment, SelectionScope};
use bmux_tui::text::Line;

use super::transcript_layout::{
    TranscriptLayoutCache, VisibleTranscriptLine, VisibleTranscriptSource,
};

/// Host-provided ephemeral styling without changing accepted geometry.
pub type RowDecorator<'a> = &'a dyn Fn(VisibleTranscriptLine, &Line) -> Line;

/// A viewport over a caller-owned accepted transcript layout.
///
/// This child neither measures source again nor calls a plugin. Native overlays,
/// clipboard I/O and action execution remain with the host. `selection_scope`
/// must be stable across frames and unique within the retained root.
pub struct TranscriptChild<'a> {
    /// Stable identity in the host's one retained component tree.
    pub id: &'a str,
    /// Complete accepted rows and corresponding source geometry.
    pub layout: &'a TranscriptLayoutCache,
    /// Items supplying stable source identity and accepted revision.
    pub items: super::transcript_document::TranscriptItems<'a>,
    /// Oldest visible document row.
    pub top: usize,
    /// Source selection scope shared with the host's copy resolver.
    pub selection_scope: &'a str,
    /// Register source geometry and actions. Native hosts migrating an existing
    /// selection scene can retain that scene until their root switches atomically.
    pub metadata: bool,
    /// Optional ephemeral row styling, without source reflow.
    pub decorate: Option<RowDecorator<'a>>,
}

impl Component for TranscriptChild<'_> {
    fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new(self.id.to_owned()),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(0),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        if width == 0 || height == 0 {
            return;
        }
        if self.metadata {
            cx.push_selection_scope(SelectionScope::new(
                self.selection_scope,
                Rect::new(0, 0, width, height),
            ));
        }
        for visible in self.layout.visible_lines_from_top(self.top, height) {
            let Some(line) = self.layout.line(visible) else {
                continue;
            };
            let y = u16::try_from(visible.row_index.saturating_sub(self.top)).unwrap_or(height);
            let decorated = self.decorate.map(|decorate| decorate(visible, line));
            let line = decorated.as_ref().unwrap_or(line);
            cx.write_line(LocalRect::new(0, i64::from(y), width, 1), line);
            if self.metadata
                && visible.source == VisibleTranscriptSource::Transcript
                && let Some(item) = self.items.get(visible.entry_index)
            {
                let mut region = bmux_tui::semantic::SemanticRegion::new(
                    format!(
                        "{}:{}:{}",
                        self.selection_scope,
                        item.id().get(),
                        visible.row_in_entry
                    ),
                    Rect::new(0, y, width, 1),
                    "text",
                );
                region.accessibility = Some(bmux_tui::semantic::SemanticAccessibility {
                    text: Some(
                        bmux_tui::text::line_viewport(line, 0, usize::from(width)).plain_text(),
                    ),
                    ..Default::default()
                });
                cx.push_semantic(region);
            }
            if !self.metadata || visible.source != VisibleTranscriptSource::Transcript {
                continue;
            }
            if let Some((projection, body_start)) = self.layout.markdown(visible.entry_index)
                && let Some(document_row) = visible.row_in_entry.checked_sub(body_start)
                && let Ok(document_row) = u16::try_from(document_row)
            {
                let Some(item) = self.items.get(visible.entry_index) else {
                    continue;
                };
                let content_id = format!("transcript:{}", item.id().get());
                for retained in self
                    .layout
                    .selection_fragments(visible.entry_index, visible.row_in_entry)
                {
                    let mut fragment = retained.clone();
                    let suffix = fragment
                        .content_id
                        .as_str()
                        .strip_prefix("retained")
                        .unwrap_or("");
                    fragment.content_id = bmux_tui::selection::SelectionContentId::new(format!(
                        "{content_id}{suffix}"
                    ));
                    fragment.scope_id =
                        bmux_tui::selection::SelectionScopeId::new(self.selection_scope);
                    fragment.id = bmux_tui::selection::SelectionFragmentId::new(format!(
                        "{}:{}:{}",
                        fragment.content_id.as_str(),
                        fragment.source_range.start,
                        fragment.source_range.end
                    ));
                    fragment.revision = item.revision();
                    fragment.area.y = y;
                    fragment.order = u64::try_from(visible.entry_index)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(1_000_000)
                        .saturating_add(fragment.order);
                    cx.push_selection_fragment(fragment);
                }
                Self::paint_markdown_actions(item, projection, document_row, y, width, cx);
            }
            self.paint_source_selection(visible, y, width, cx);
        }
    }
}

impl TranscriptChild<'_> {
    fn paint_markdown_actions(
        item: &super::item::TranscriptItem,
        projection: &bcode_markdown_render::MarkdownRenderResult,
        document_row: u16,
        y: u16,
        width: u16,
        cx: &mut PaintCx<'_, '_>,
    ) {
        for (block_index, block) in projection.code_block_selections().iter().enumerate() {
            let Some(geometry) = projection
                .code_block_geometry
                .iter()
                .find(|geometry| geometry.contribution_id == format!("code-header:{block_index}"))
            else {
                continue;
            };
            for rect in &geometry.rects {
                if document_row >= rect.y && document_row < rect.y.saturating_add(rect.height) {
                    cx.push_hit(
                        bmux_tui::hit::HitRegion::new(
                            format!(
                                "transcript-copy:{}:{}:{width}:{}",
                                item.id().get(),
                                item.revision(),
                                block.id
                            ),
                            Rect::new(rect.x.saturating_add(2), y, rect.width, 1),
                        )
                        .role(bmux_tui::hit::HitRole::Action)
                        .layer(1),
                    );
                }
            }
        }
        for geometry in &projection.geometry {
            if !projection.contributions.iter().any(|contribution| {
                contribution.id == geometry.contribution_id
                    && (crate::transcript_markdown::contribution_actionable(&contribution.kind)
                        || matches!(
                            contribution.kind,
                            bcode_markdown_render::MarkdownContributionKind::Image { .. }
                        ))
            }) {
                continue;
            }
            for (index, rect) in geometry.rects.iter().enumerate() {
                if document_row < rect.y || document_row >= rect.y.saturating_add(rect.height) {
                    continue;
                }
                cx.push_hit(
                    bmux_tui::hit::HitRegion::new(
                        format!(
                            "markdown:{}:{}:{}:{}:{index}",
                            item.id().get(),
                            item.revision(),
                            width,
                            geometry.contribution_id
                        ),
                        Rect::new(rect.x.saturating_add(2), y, rect.width, 1),
                    )
                    .role(bmux_tui::hit::HitRole::Action)
                    .layer(1),
                );
            }
        }
    }

    fn paint_source_selection(
        &self,
        visible: VisibleTranscriptLine,
        y: u16,
        width: u16,
        cx: &mut PaintCx<'_, '_>,
    ) {
        let Some(source) = self
            .layout
            .selection_row(visible.entry_index, visible.row_in_entry)
        else {
            return;
        };
        if !source.is_valid() {
            return;
        }
        for (index, cell) in source.cells.iter().enumerate() {
            if cell.column >= width || cell.width > width - cell.column {
                continue;
            }
            cx.push_selection_fragment(
                SelectionFragment::new(
                    self.selection_scope,
                    source.identity.clone(),
                    Rect::new(cell.column, y, cell.width, 1),
                    u64::try_from(visible.row_index)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(1_000_000)
                        .saturating_add(u64::try_from(index).unwrap_or(u64::MAX)),
                    cell.bytes.clone(),
                )
                .revision(source.revision),
            );
        }
    }
}
