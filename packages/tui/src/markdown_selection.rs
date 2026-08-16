//! Transcript Markdown selection provenance projected into BMUX selection fragments.

use std::ops::Range;

use bcode_markdown_render::{
    MarkdownCodeBlockSelection, MarkdownRenderResult, MarkdownSelectionProvenance,
};
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
    for (block_index, block) in rendered.code_block_selections.iter().enumerate() {
        let body_rects = rendered
            .selection_provenance
            .iter()
            .filter(|unit| {
                unit.source_ranges
                    .iter()
                    .all(|range| range_within_any(range, &block.body_ranges))
            })
            .flat_map(|unit| &unit.rects)
            .collect::<Vec<_>>();
        let Some(first_row) = body_rects.iter().map(|rect| rect.y).min() else {
            continue;
        };
        let Some(last_row) = body_rects.iter().map(|rect| rect.y).max() else {
            continue;
        };
        let width = body_rects
            .iter()
            .map(|rect| rect.x.saturating_add(rect.width))
            .max()
            .unwrap_or(1)
            .max(1);
        let whole_content_id = format!("{base_content_id}.code.{}.whole", block.whole_range.start);
        let base_order = order_base
            .saturating_add(u64::try_from(block_index).unwrap_or(u64::MAX))
            .saturating_mul(1_000_000);
        for (row_index, row) in (first_row..=last_row).enumerate() {
            fragments.push(
                SelectionFragment::new(
                    scope_id,
                    whole_content_id.clone(),
                    bmux_tui::geometry::Rect::new(origin.x, origin.y.saturating_add(row), 2, 1),
                    base_order.saturating_add(u64::try_from(row_index).unwrap_or(u64::MAX)),
                    block.whole_range.clone(),
                )
                .revision(revision),
            );
        }
        if let Some(header_range) = &block.header_range {
            fragments.push(
                SelectionFragment::new(
                    scope_id,
                    format!("{base_content_id}.code.{}.header", block.whole_range.start),
                    bmux_tui::geometry::Rect::new(
                        origin.x,
                        origin.y.saturating_add(first_row.saturating_sub(1)),
                        width,
                        1,
                    ),
                    base_order.saturating_add(100_000),
                    header_range.clone(),
                )
                .revision(revision),
            );
        }
        if let Some(footer_range) = &block.footer_range {
            fragments.push(
                SelectionFragment::new(
                    scope_id,
                    format!("{base_content_id}.code.{}.footer", block.whole_range.start),
                    bmux_tui::geometry::Rect::new(
                        origin.x,
                        origin.y.saturating_add(last_row.saturating_add(1)),
                        width,
                        1,
                    ),
                    base_order.saturating_add(200_000),
                    footer_range.clone(),
                )
                .revision(revision),
            );
        }
    }
}

fn markdown_unit_content_id(
    base: &str,
    rendered: &MarkdownRenderResult,
    unit: &MarkdownSelectionProvenance,
) -> String {
    rendered
        .code_block_selections
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

fn range_within_any(range: &Range<usize>, parents: &[Range<usize>]) -> bool {
    parents.iter().any(|parent| range_within(range, parent))
}

pub fn expand_markdown_selection_range(
    source: &str,
    content_suffix: &str,
    selected: Range<usize>,
) -> Option<Vec<Range<usize>>> {
    let suffix = content_suffix.strip_prefix("code.")?;
    let (block_start, selection_kind) = suffix.split_once('.')?;
    let block_start = block_start.parse::<usize>().ok()?;
    let block = bcode_markdown_render::markdown_code_block_selections(source)
        .into_iter()
        .find(|block| block.whole_range.start == block_start)?;
    code_selection_ranges(&block, selection_kind, selected)
}

fn code_selection_ranges(
    block: &MarkdownCodeBlockSelection,
    selection_kind: &str,
    selected: Range<usize>,
) -> Option<Vec<Range<usize>>> {
    match selection_kind {
        "body" => Some(intersect_ranges(&block.body_ranges, selected)),
        "header" => Some(intersect_optional_range(
            block.header_range.as_ref(),
            selected,
        )),
        "footer" => Some(intersect_optional_range(
            block.footer_range.as_ref(),
            selected,
        )),
        "whole" => Some(intersect_ranges(
            std::slice::from_ref(&block.whole_range),
            selected,
        )),
        _ => None,
    }
}

fn intersect_optional_range(
    range: Option<&Range<usize>>,
    selected: Range<usize>,
) -> Vec<Range<usize>> {
    range.map_or_else(Vec::new, |range| {
        intersect_ranges(std::slice::from_ref(range), selected)
    })
}

fn intersect_ranges(ranges: &[Range<usize>], selected: Range<usize>) -> Vec<Range<usize>> {
    ranges
        .iter()
        .filter_map(|range| {
            let start = range.start.max(selected.start);
            let end = range.end.min(selected.end);
            (start < end).then_some(start..end)
        })
        .collect()
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
            Some(vec![0..10, 10..18])
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
        assert!(fragments.iter().any(|fragment| {
            fragment.content_id.as_str() == "bcode.transcript.item.7.markdown.code.0.whole"
        }));
    }
}
