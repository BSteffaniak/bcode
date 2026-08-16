//! Transcript Markdown selection provenance projected into BMUX selection fragments.

use bcode_markdown_render::{MarkdownRenderResult, MarkdownSelectionProvenance};
use bmux_tui::selection::SelectionFragment;

pub fn markdown_selection_fragments(
    scope_id: &str,
    content_id: &str,
    rendered: &MarkdownRenderResult,
    origin: bmux_tui::geometry::Point,
    order_base: u64,
    revision: u64,
) -> Vec<SelectionFragment> {
    let mut fragments = Vec::new();
    for (unit_index, unit) in rendered.selection_provenance.iter().enumerate() {
        append_unit_fragments(
            &mut fragments,
            scope_id,
            content_id,
            unit,
            unit_index,
            origin,
            order_base,
            revision,
        );
    }
    fragments
}

#[allow(clippy::too_many_arguments)]
fn append_unit_fragments(
    fragments: &mut Vec<SelectionFragment>,
    scope_id: &str,
    content_id: &str,
    unit: &MarkdownSelectionProvenance,
    unit_index: usize,
    origin: bmux_tui::geometry::Point,
    order_base: u64,
    revision: u64,
) {
    if unit.rects.len() != unit.source_ranges.len() {
        return;
    }
    for (part_index, (rect, source_range)) in unit.rects.iter().zip(&unit.source_ranges).enumerate()
    {
        if rect.width == 0 || rect.height == 0 || source_range.is_empty() {
            continue;
        }
        fragments.push(
            SelectionFragment::new(
                scope_id,
                content_id,
                bmux_tui::geometry::Rect::new(
                    origin.x.saturating_add(rect.x),
                    origin.y.saturating_add(rect.y),
                    rect.width,
                    rect.height,
                ),
                order_base
                    .saturating_add(u64::try_from(unit_index).unwrap_or(u64::MAX))
                    .saturating_add(u64::try_from(part_index).unwrap_or(u64::MAX)),
                source_range.clone(),
            )
            .revision(revision),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_canonical_graphemes_at_transcript_origin() {
        let rendered = bcode_markdown_render::render_markdown(
            "**wide 界**",
            &bcode_markdown_render::MarkdownRenderOptions::new(20),
        );
        let fragments = markdown_selection_fragments(
            "item",
            "item.markdown",
            &rendered,
            bmux_tui::geometry::Point::new(4, 7),
            10,
            2,
        );

        assert!(!fragments.is_empty());
        assert!(fragments.iter().all(|fragment| fragment.area.x >= 4));
        assert!(fragments.iter().all(|fragment| fragment.area.y >= 7));
        assert!(fragments.iter().any(|fragment| fragment.area.width == 2));
    }
}
