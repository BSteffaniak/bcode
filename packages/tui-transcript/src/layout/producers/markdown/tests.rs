//! Markdown body behavior: row-count-preserving containment (B10), source mapping from the
//! retained projection (B6, C24), document-scoped `<details>` state and work reporting.

use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::text_width::display_width;

use super::*;
use crate::layout::context::RenderConfig;
use crate::units::structure::ResolvedUnitPresentation;
use crate::units::{BoundaryEdge, UnitKey};

fn layout(content: &dyn ItemContent, width: u64) -> LayoutNode {
    content.layout(Constraints::for_width(width), &mut LayoutCx::new())
}

fn paint(content: &dyn ItemContent, node: &LayoutNode) -> Vec<String> {
    let width = u16::try_from(node.size.width).expect("width");
    let height = u16::try_from(node.size.height).expect("height");
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    content.paint(node, &mut PaintCx::new(&mut frame));
    (0..height)
        .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
        .collect()
}

fn options(config: &RenderConfig, request: &MarkdownRequest<'_>) -> MarkdownRenderOptions {
    let structure = ResolvedUnitPresentation::Boundary;
    let cx = config.context(
        &UnitKey::Boundary(BoundaryEdge::Older),
        request.width,
        &structure,
    );
    render_options(request, &cx)
}

fn project(config: &RenderConfig, request: &MarkdownRequest<'_>) -> (MarkdownBody, ProducerWork) {
    let structure = ResolvedUnitPresentation::Boundary;
    let cx = config.context(
        &UnitKey::Boundary(BoundaryEdge::Older),
        request.width,
        &structure,
    );
    markdown_projection(request, &cx)
}

#[test]
fn b10_container_keeps_projection_rows_and_clips_overflow() {
    let config = RenderConfig::default();
    let source =
        "# Title\n\nsome words that wrap\n\n- item one\n- item two\n\n```rust\nfn main() {}\n```";
    let (body, work) = project(&config, &MarkdownRequest::new(source, 30));
    assert_eq!(work.markdown_parses, 1);
    assert_eq!(work.projections, 1);
    assert!(
        work.highlights >= 1,
        "the fenced code block counts as a highlight"
    );
    let at_width = layout(&body, 30);
    assert_eq!(
        usize::try_from(at_width.size.height).expect("rows"),
        body.rows()
    );
    // Laid out narrower than projected, the body keeps its row count and clips (B10).
    let narrow = layout(&body, 6);
    assert_eq!(narrow.size.height, at_width.size.height);
    for row in paint(&body, &narrow) {
        assert!(display_width(&row) <= 6, "{row:?}");
    }
}

#[test]
fn b6_c24_source_rows_come_from_the_retained_projection() {
    let config = RenderConfig::default();
    let source = "one two three four five six seven eight nine ten\n\nsecond paragraph here";
    let second = source.find("second").expect("offset") as u64;
    for width in [80, 16, 8] {
        let (body, _) = project(&config, &MarkdownRequest::new(source, width));
        let node = layout(&body, width);
        let geometry = body.geometry(&node);
        let rows = paint(&body, &node);
        let row = geometry
            .source_map
            .row_for_source(&SourcePosition::MarkdownByte(second))
            .expect("mapped");
        assert!(
            rows[usize::try_from(row).expect("row")].contains("second"),
            "{rows:?}"
        );
        let bytes: Vec<_> = geometry
            .source_map
            .rows()
            .iter()
            .map(|row| row.position.clone())
            .collect();
        assert!(bytes.windows(2).all(|pair| pair[0] <= pair[1]), "{bytes:?}");
        assert_eq!(geometry.selection_fragments[0].rows, 0..node.size.height);
    }
}

#[test]
fn details_state_is_scoped_to_the_owning_document() {
    let source = "<details><summary>More</summary>\n\nhidden body text\n\n</details>";
    let request = MarkdownRequest::new(source, 40).document("transcript:item:a");
    let mut config = RenderConfig::default();
    let (closed, _) = project(&config, &request);
    let id = render_markdown(source, &options(&config, &request))
        .contributions
        .iter()
        .find(|contribution| matches!(contribution.kind, MarkdownContributionKind::Details { .. }))
        .map(|contribution| contribution.id.clone())
        .expect("details contribution");
    assert!(id.starts_with("transcript:item:a:"), "{id}");
    // Another document's state does not apply.
    config
        .markdown
        .details_open
        .insert("transcript:item:b:x".to_owned(), true);
    let (unrelated, _) = project(&config, &request);
    assert_eq!(unrelated.rows(), closed.rows());
    config.markdown.details_open.insert(id, true);
    let (open, _) = project(&config, &request);
    assert!(
        open.rows() > closed.rows(),
        "opening details adds the body rows"
    );
}
