//! Producer helper behavior over the B8 matrix: zero and tiny widths, oversized chrome, CJK,
//! emoji, combining marks, styled spans, and agreement between measured rows, painted cells
//! and derived geometry (B2–B6).

use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::Color;
use bmux_tui::text::TextWrap;
use bmux_tui::text_width::display_width;

use super::*;

fn layout(content: &dyn ItemContent, width: u64) -> LayoutNode {
    content.layout(Constraints::for_width(width), &mut LayoutCx::new())
}

/// Lay out at `width` and paint into a buffer of exactly the measured size.
fn paint(content: &dyn ItemContent, width: u16) -> (LayoutNode, Vec<String>) {
    let node = layout(content, u64::from(width));
    let height = u16::try_from(node.size.height).expect("height");
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    content.paint(&node, &mut PaintCx::new(&mut frame));
    let rows = (0..height)
        .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
        .collect();
    (node, rows)
}

fn panel(padding_x: u16, padding_y: u16) -> TranscriptContainerStyle {
    TranscriptContainerStyle {
        recipe: TranscriptContainerRecipe {
            layout: TranscriptContainerLayout::Panel,
            width: TranscriptContainerWidth::Full,
            border: TranscriptContainerBorder::All,
            padding_x,
            padding_y,
        },
        style: Style::new().bg(Color::Blue),
        border_set: BorderSet::SINGLE,
    }
}

fn text(source: &str, wrap: TextWrap) -> Box<dyn ItemContent> {
    let lines = source
        .split('\n')
        .map(|line| Line::raw(line.to_owned()))
        .collect::<Vec<_>>();
    Box::new(SourceText::new(
        TextBlock::new(Text::from_lines(lines)).wrap(wrap),
        TextSource::Markdown { base: 0 },
    ))
}

#[test]
fn u3_b8_status_rows_are_one_row_at_every_width_and_clip() {
    let row = boundary_row(BoundaryEdge::Older, BoundaryStatus::MoreAvailable);
    for width in [0, 1, 3, 80] {
        assert_eq!(layout(&row, width).size.height, 1);
    }
    let (_, rows) = paint(&row, 3);
    assert_eq!(rows, vec!["ear".to_owned()]);
}

#[test]
fn b2_b8_text_measures_terminal_cells_for_cjk_emoji_combining_marks_and_styled_spans() {
    // Each case is two rows at width 4: four two-cell graphemes, or eight one-cell graphemes
    // spelled with combining marks, or styled spans that only fit split.
    let cases: Vec<Box<dyn ItemContent>> = vec![
        text("漢字漢字", TextWrap::Character),
        text("😀😀😀😀", TextWrap::Character),
        text(&"e\u{301}".repeat(8), TextWrap::Character),
        Box::new(SourceText::new(
            TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                Span::styled("abcd", Style::new().fg(Color::Red)),
                Span::styled("efgh", Style::new().fg(Color::Green)),
            ])]))
            .wrap(TextWrap::Character),
            TextSource::Markdown { base: 0 },
        )),
    ];
    for content in &cases {
        let (node, rows) = paint(content.as_ref(), 4);
        assert_eq!(node.size.height, 2);
        assert!(rows.iter().all(|row| display_width(row) <= 4), "{rows:?}");
        let geometry = content.geometry(&node);
        assert_eq!(
            geometry.source_map.rows().len(),
            2,
            "one mapping per measured row"
        );
        assert_eq!(geometry.selection_fragments[0].rows, 0..2);
    }
    // The second CJK row starts at the third grapheme: byte 6 of the source.
    let geometry = cases[0].geometry(&layout(cases[0].as_ref(), 4));
    assert_eq!(
        geometry.source_map.source_for_row(1),
        Some((SourcePosition::MarkdownByte(6), 1))
    );
}

#[test]
fn b5_b8_unwrapped_wide_graphemes_stay_inside_tiny_widths() {
    for width in [0u16, 1, 2] {
        let row = single_row("漢字");
        let node = layout(&row, u64::from(width));
        assert_eq!(node.size.height, 1);
        if width > 0 {
            let (_, rows) = paint(&row, width);
            assert!(display_width(&rows[0]) <= usize::from(width));
        }
    }
}

#[test]
fn b4_frames_and_indents_degrade_explicitly_at_narrow_widths() {
    let style = panel(1, 0);
    assert_eq!(frame_insets(&style).horizontal(), 4);
    for width in [0, 1, 4] {
        let content = framed(text("ab", TextWrap::Word), style, width);
        let plain = text("ab", TextWrap::Word);
        assert_eq!(
            layout(content.as_ref(), width),
            layout(plain.as_ref(), width),
            "frame dropped at width {width}"
        );
    }
    let content = framed(text("ab", TextWrap::Word), style, 10);
    assert_eq!(layout(content.as_ref(), 10).size.height, 3);

    let indented = spaced(text("abc", TextWrap::Word), Spacing::default(), 4, 3);
    let node = layout(indented.as_ref(), 3);
    assert_eq!(
        node.size.height, 2,
        "content row plus one spacing row; indent dropped"
    );
}

#[test]
fn b3_b6_framed_geometry_and_paint_come_from_the_same_node() {
    let content = framed(text("hello\nworld", TextWrap::Word), panel(1, 0), 12);
    let (node, rows) = paint(content.as_ref(), 12);
    assert_eq!(node.size.height, 4);
    assert_eq!(rows[0], "┌──────────┐");
    assert_eq!(rows[1], "│ hello    │");
    assert_eq!(rows[3], "└──────────┘");
    let geometry = content.geometry(&node);
    assert_eq!(
        geometry.source_map.source_for_row(2),
        Some((SourcePosition::MarkdownByte(6), 2)),
        "rows shift by the top border"
    );
    assert_eq!(geometry.selection_fragments[0].rows, 1..3);
}

#[test]
fn header_lines_fit_their_width_in_cells() {
    for width in 0..24 {
        let line = header_line(
            vec![
                HeaderSegment::required("▸ 漢字 group", Style::new()).truncatable(),
                HeaderSegment::optional("3 tools", Style::new(), 1),
                HeaderSegment::optional("12s", Style::new(), 2),
            ],
            " · ",
            width,
        );
        assert!(
            line.width() <= usize::try_from(width).expect("width") || width < 2,
            "{width}: {line:?}"
        );
    }
}

#[test]
fn markers_fall_back_to_ascii_without_icons() {
    assert_eq!(
        disclosure_marker(Disclosure::Expanded, IconSet::Unicode),
        "▾"
    );
    assert_eq!(disclosure_marker(Disclosure::Collapsed, IconSet::None), ">");
    assert_eq!(status_glyph(Some(UnitStatus::Failed), IconSet::None), "x");
    assert_eq!(hidden_marker_text(3, IconSet::Unicode), "▸ 3 hidden");
    assert_eq!(elapsed_label(65_000), "1m05s");
}

#[test]
fn container_style_reads_theme_recipes_and_tool_borders() {
    let mut theme = ResolvedThemeDefinition::empty("t", "T");
    theme.containers.insert(
        "tool.failed".to_owned(),
        bcode_tui_theme::definition::ContainerRecipe {
            layout: ContainerLayout::Panel,
            width: ContainerWidth::Full,
            border: ContainerBorder::All,
            padding_x: 1,
            padding_y: 0,
        },
    );
    theme.chrome.borders.tool = Some(BorderStyle::Rounded);
    let style = container_style(&theme, tool_container_role(Some(UnitStatus::Failed)), "x");
    assert_eq!(style.recipe.layout, TranscriptContainerLayout::Panel);
    assert_eq!(style.border_set, BorderSet::ROUNDED);
    let plain = container_style(&theme, "transcript.user", "x");
    assert_eq!(plain.recipe.layout, TranscriptContainerLayout::Plain);
}
