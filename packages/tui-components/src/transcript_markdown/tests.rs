use super::*;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::{Point, Size};

#[test]
fn retained_projection_keeps_original_fence_source_and_identity_across_resize() {
    let source = "# 你好\n\n[a link](https://example.com)\n\n```rust\r\nlet x = 1;\r\n```\r\n";
    for width in [4, 12, 80] {
        let options = MarkdownRenderOptions::new(width).with_document_id("session/item-7");
        let accepted = TranscriptMarkdown::prepare("item-7".into(), 42, source.into(), &options);
        assert_eq!(accepted.content_id(), "item-7");
        assert_eq!(accepted.revision(), 42);
        assert_eq!(accepted.source(), source);
        assert!(accepted.projection().has_selection_source(source));
        let fragments = crate::markdown_selection::markdown_selection_fragments(
            "transcript",
            accepted.content_id(),
            accepted.projection(),
            Point::new(0, 0),
            0,
            42,
            None,
        );
        assert!(!fragments.is_empty());
        assert!(
            accepted
                .projection()
                .contributions
                .iter()
                .any(|c| contribution_actionable(&c.kind))
        );
    }
}

#[test]
fn native_block_reports_the_exact_body_offset_and_retains_styled_projection() {
    let accepted = TranscriptMarkdown::prepare(
        "item".into(),
        1,
        "**bold** and `code`".into(),
        &MarkdownRenderOptions::new(30),
    );
    let mut rows = vec![Line::raw("previous item")];
    let start = append_markdown_block(
        &mut rows,
        "Assistant",
        accepted.projection(),
        32,
        MarkdownBlockStyle {
            heading: Style::new(),
            muted: Style::new(),
            prominent: true,
            body_indent: 2,
        },
    );
    assert_eq!(start, 2);
    for (index, line) in accepted.projection().lines.iter().enumerate() {
        assert_eq!(
            rows[start + index].plain_text(),
            format!("  {}", line.plain_text())
        );
        assert_eq!(&rows[start + index].spans[1..], line.spans.as_slice());
    }
}

#[test]
fn body_layout_respects_tiny_constraints_without_reprojecting_source() {
    let accepted = TranscriptMarkdown::prepare(
        "item".into(),
        9,
        "你好 👩‍💻 e\u{301}".into(),
        &MarkdownRenderOptions::new(12),
    );
    for width in [0, 1, 2, 12] {
        for height in [0, 1, 8] {
            let body = MarkdownBody::new("body", "scope".into(), &accepted, 0).top_row(1);
            let layout = body.layout(
                Constraints::tight(Size::new(width, height)),
                &mut LayoutCx::new(),
            );
            assert_eq!(layout.size.width, u64::from(width));
            assert_eq!(layout.size.height, u64::from(height));
            assert_eq!(accepted.source(), "你好 👩‍💻 e\u{301}");
        }
    }
}

#[test]
fn scrolled_body_clips_selection_and_hits_through_the_same_bmux_context() {
    use bmux_tui::{buffer::Buffer, frame::Frame, geometry::Rect, paint::PaintCx};
    let accepted = TranscriptMarkdown::prepare(
        "item".into(),
        12,
        "first\n\n[a safe link](https://example.com)\n\nlast".into(),
        &MarkdownRenderOptions::new(20).with_document_id("item"),
    );
    let body = MarkdownBody::new("body", "scope".into(), &accepted, 0).top_row(1);
    let area = Rect::new(0, 0, 20, 2);
    let layout = body.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    body.paint(&layout, &mut PaintCx::new(&mut frame));
    assert!(!frame.hits().regions().is_empty());
    for fragment in frame.selection().fragments() {
        assert_eq!(fragment.area.intersection(area), fragment.area);
        assert_eq!(fragment.revision, 12);
        assert!(
            accepted
                .source()
                .get(fragment.source_range.clone())
                .is_some()
        );
    }
    for hit in frame.hits().regions() {
        assert_eq!(hit.area.intersection(area), hit.area);
    }
}
