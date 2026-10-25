//! Transcript Markdown selection provenance projected into BMUX selection fragments.

use std::ops::Range;

use bcode_markdown_render::{MarkdownRenderResult, MarkdownSelectionProvenance};
use bmux_tui::selection::SelectionFragment;

/// Return selection fragments for one rendered Markdown projection.
///
/// `document_row` restricts derivation to a single document row. Per-frame
/// rendering needs exactly one row, so restricting avoids building unnecessary
/// whole-document fragments. Source byte ordering is shared by body, prose, and
/// fence fragments; row filtering never changes the document order.
/// Empty code rows currently have no exact source endpoint anchors; BMUX's
/// nearest-fragment gesture fallback must not be interpreted as precise blank-row
/// selection. Intervening unpainted source breaks remain part of body intervals.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn markdown_selection_fragments(
    scope_id: &str,
    content_id: &str,
    rendered: &MarkdownRenderResult,
    origin: bmux_tui::geometry::Point,
    order_base: u64,
    revision: u64,
    document_row: Option<u16>,
) -> Vec<SelectionFragment> {
    let mut fragments = Vec::new();
    for (unit_index, unit) in rendered.selection_provenance().iter().enumerate() {
        if let Some(document_row) = document_row
            && !unit.rects.iter().any(|rect| rect.y == document_row)
        {
            continue;
        }
        let content_id = markdown_unit_content_id(content_id, rendered, unit);
        append_unit_fragments(
            &mut fragments,
            scope_id,
            &content_id,
            unit,
            unit_index,
            origin,
            order_base,
            revision,
        );
    }
    append_code_block_chrome_fragments(
        &mut fragments,
        scope_id,
        content_id,
        rendered,
        origin,
        order_base,
        revision,
    );
    fragments
}

fn append_code_block_chrome_fragments(
    fragments: &mut Vec<SelectionFragment>,
    scope_id: &str,
    base_content_id: &str,
    rendered: &MarkdownRenderResult,
    origin: bmux_tui::geometry::Point,
    order_base: u64,
    revision: u64,
) {
    if rendered
        .code_block_geometry
        .iter()
        .filter(|geometry| geometry.contribution_id.starts_with("code-block:"))
        .count()
        != rendered.code_block_selections().len()
    {
        // Nested/projected blocks without canonical correspondence cannot claim fences.
        return;
    }
    for (block_index, block) in rendered.code_block_selections().iter().enumerate() {
        for (kind, range) in [
            ("header", &block.header_range),
            ("footer", &block.footer_range),
        ] {
            let Some(range) = range else { continue };
            let id = format!("code-{kind}:{block_index}");
            let Some(geometry) = rendered
                .code_block_geometry
                .iter()
                .find(|geometry| geometry.contribution_id == id)
            else {
                continue;
            };
            for rect in &geometry.rects {
                fragments.push(
                    SelectionFragment::new(
                        scope_id,
                        format!("{base_content_id}.code.{}.{kind}", block.whole_range.start),
                        bmux_tui::geometry::Rect::new(
                            origin.x.saturating_add(rect.x),
                            origin.y.saturating_add(rect.y),
                            rect.width,
                            rect.height,
                        ),
                        order_base.saturating_add(u64::try_from(range.start).unwrap_or(u64::MAX)),
                        range.clone(),
                    )
                    .revision(revision),
                );
            }
        }
    }
}

fn markdown_unit_content_id(
    base: &str,
    rendered: &MarkdownRenderResult,
    unit: &MarkdownSelectionProvenance,
) -> String {
    rendered
        .code_block_selections()
        .iter()
        .find(|block| {
            unit.source_ranges
                .iter()
                .all(|range| range_within(range, &block.whole_range))
        })
        .map_or_else(
            || base.to_owned(),
            |block| format!("{base}.code.{}.body", block.whole_range.start),
        )
}

const fn range_within(range: &Range<usize>, parent: &Range<usize>) -> bool {
    parent.start <= range.start && range.end <= parent.end
}

/// Expand a source selection using the canonical Markdown fence ranges.
#[must_use]
pub fn expand_markdown_selection_range(
    source: &str,
    content_suffix: &str,
    selected: Range<usize>,
) -> Option<Vec<Range<usize>>> {
    let blocks = bcode_markdown_render::markdown_code_block_selections(source);
    expand_code_selection_range(source, &blocks, content_suffix, selected)
}

/// Resolve a code body or fence selection without inventing source bytes.
#[must_use]
pub fn expand_code_selection_range(
    source: &str,
    blocks: &[bcode_markdown_render::MarkdownCodeBlockSelection],
    content_suffix: &str,
    selected: Range<usize>,
) -> Option<Vec<Range<usize>>> {
    use bcode_markdown_render::MarkdownCodeSelectionContent;
    let suffix = content_suffix.strip_prefix("code.")?;
    let (block_start, selection_kind) = suffix.split_once('.')?;
    let block_start = block_start.parse::<usize>().ok()?;
    let block = blocks
        .iter()
        .find(|block| block.whole_range.start == block_start)?;
    let content = match selection_kind {
        "body" => MarkdownCodeSelectionContent::Body,
        "header" => MarkdownCodeSelectionContent::Header,
        "footer" => MarkdownCodeSelectionContent::Footer,
        "whole" => MarkdownCodeSelectionContent::Whole,
        _ => return None,
    };
    block.selection_ranges(source, content, selected.start, selected.end)
}

#[allow(clippy::too_many_arguments)]
fn append_unit_fragments(
    fragments: &mut Vec<SelectionFragment>,
    scope_id: &str,
    content_id: &str,
    unit: &MarkdownSelectionProvenance,
    _unit_index: usize,
    origin: bmux_tui::geometry::Point,
    order_base: u64,
    revision: u64,
) {
    if unit.rects.len() != unit.source_ranges.len() {
        return;
    }
    for (rect, source_range) in unit.rects.iter().zip(&unit.source_ranges) {
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
                order_base.saturating_add(u64::try_from(source_range.start).unwrap_or(u64::MAX)),
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
    fn expands_code_body_without_fences_or_invented_bytes() {
        let source = "```rust\r\nfn main() {}\r\n```\r\n";
        assert_eq!(
            expand_markdown_selection_range(source, "code.0.body", 0..source.len()),
            Some(std::iter::once(9..23).collect())
        );
        assert_eq!(
            expand_markdown_selection_range(source, "code.0.header", 0..source.len()),
            Some(std::iter::once(0..9).collect())
        );
        assert_eq!(
            expand_markdown_selection_range(source, "code.0.footer", 0..source.len()),
            Some(std::iter::once(23..source.len()).collect())
        );
        assert_eq!(
            expand_markdown_selection_range(source, "code.0.whole", 0..source.len()),
            Some(std::iter::once(0..source.len()).collect())
        );

        let indented = "    first\n\tsecond\n\nnext";
        assert_eq!(
            expand_markdown_selection_range(indented, "code.0.body", 0..indented.len(),),
            Some(std::iter::once(0..19).collect())
        );
        assert_eq!(
            expand_markdown_selection_range(indented, "code.0.whole", 0..indented.len(),),
            Some(std::iter::once(0..19).collect())
        );

        let incomplete = "```\n\tbody";
        assert_eq!(
            expand_markdown_selection_range(incomplete, "code.0.body", 0..incomplete.len(),)
                .and_then(|ranges| ranges.first().cloned()),
            Some(4..9)
        );
    }

    #[test]
    fn markdown_code_partial_ranges_never_expand_beyond_selection() {
        let source = "```rust\nabcdef\n```\n";
        assert_eq!(
            expand_markdown_selection_range(source, "code.0.body", 12..14),
            Some(std::iter::once(12..14).collect())
        );
        assert_eq!(
            expand_markdown_selection_range(source, "code.0.header", 3..7),
            Some(std::iter::once(3..7).collect())
        );
        assert_eq!(
            expand_markdown_selection_range(source, "code.0.footer", 17..19),
            Some(std::iter::once(17..19).collect())
        );
    }

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
            None,
        );

        assert!(!fragments.is_empty());
        assert!(fragments.iter().all(|fragment| fragment.area.x >= 4));
        assert!(fragments.iter().all(|fragment| fragment.area.y >= 7));
        assert!(fragments.iter().any(|fragment| fragment.area.width == 2));
    }

    #[test]
    fn code_body_fragments_use_semantic_content_identity() {
        let rendered = bcode_markdown_render::render_markdown(
            "```rust\nlet value = 1;\n```",
            &bcode_markdown_render::MarkdownRenderOptions::new(40),
        );
        let fragments = markdown_selection_fragments(
            "item",
            "bcode.transcript.item.7.markdown",
            &rendered,
            bmux_tui::geometry::Point::new(0, 0),
            0,
            1,
            None,
        );

        assert!(fragments.iter().any(|fragment| {
            fragment.content_id.as_str() == "bcode.transcript.item.7.markdown.code.0.body"
        }));
        assert!(fragments.iter().any(|fragment| {
            fragment.content_id.as_str() == "bcode.transcript.item.7.markdown.code.0.header"
        }));
        assert!(fragments.iter().any(|fragment| {
            fragment.content_id.as_str() == "bcode.transcript.item.7.markdown.code.0.footer"
        }));
        assert!(!fragments.iter().any(|fragment| {
            fragment.content_id.as_str() == "bcode.transcript.item.7.markdown.code.0.whole"
        }));
    }
}
