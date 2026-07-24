use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};
use bmux_tui::prelude::{Color, Line, Modifier, Style};
use unicode_width::UnicodeWidthStr;

const BLOCKQUOTES: &str = include_str!("fixtures/blockquotes.md");
const BLOCKS: &str = include_str!("fixtures/blocks.md");
const CODE_BLOCKS: &str = include_str!("fixtures/code_blocks.md");
const COMPOSITION: &str = include_str!("fixtures/composition.md");
const HEADINGS: &str = include_str!("fixtures/headings.md");
const HTML_XSS: &str = include_str!("fixtures/html_xss.md");
const IMAGES: &str = include_str!("fixtures/images.md");
const INLINE: &str = include_str!("fixtures/inline.md");
const INLINE_NESTED: &str = include_str!("fixtures/inline_nested.md");
const LINKS_IMAGES: &str = include_str!("fixtures/links_images.md");
const LISTS: &str = include_str!("fixtures/lists.md");
const LISTS_NUMBERING: &str = include_str!("fixtures/lists_numbering.md");
const LISTS_TIGHT_LOOSE: &str = include_str!("fixtures/lists_tight_loose.md");
const PARAGRAPHS: &str = include_str!("fixtures/paragraphs.md");
const STREAMING_CODE: &str = include_str!("fixtures/streaming_code.md");
const STREAMING_INLINE: &str = include_str!("fixtures/streaming_inline.md");
const STREAMING_STRUCTURES: &str = include_str!("fixtures/streaming_structures.md");
const TABLE_FORMATTED: &str = include_str!("fixtures/table_formatted.md");
const TABLE_MULTIPLE_ROWS: &str = include_str!("fixtures/table_multiple_rows.md");
const TABLE_SINGLE_ROW: &str = include_str!("fixtures/table_single_row.md");
const TABLE_UNEVEN: &str = include_str!("fixtures/table_uneven.md");
const TABLE_UNICODE: &str = include_str!("fixtures/table_unicode.md");
const UNICODE: &str = include_str!("fixtures/unicode.md");

fn render(markdown: &str, width: u16) -> Vec<Line> {
    render_markdown_lines(markdown, MarkdownRenderOptions::new(width))
}

fn visible_snapshot(markdown: &str, width: u16) -> String {
    visible_lines_snapshot(&render(markdown, width))
}

fn visible_lines_snapshot(lines: &[Line]) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            format!("{index:02} │ {text}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn styled_snapshot(markdown: &str, width: u16) -> String {
    render(markdown, width)
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let spans = compact_spans(line)
                .into_iter()
                .map(|(content, style)| {
                    if style == Style::new() {
                        content
                    } else {
                        format!("[{}]{content}[/]", style_label(style))
                    }
                })
                .collect::<String>();
            format!("{index:02} │ {spans}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_spans(line: &Line) -> Vec<(String, Style)> {
    let mut output: Vec<(String, Style)> = Vec::new();
    for span in &line.spans {
        if let Some((content, _)) = output.last_mut().filter(|(_, style)| *style == span.style) {
            content.push_str(&span.content);
        } else {
            output.push((span.content.clone(), span.style));
        }
    }
    output
}

fn style_label(style: Style) -> String {
    let mut attributes = Vec::new();
    if let Some(fg) = style.fg {
        attributes.push(format!("fg={}", color_label(fg)));
    }
    if let Some(bg) = style.bg {
        attributes.push(format!("bg={}", color_label(bg)));
    }
    for (modifier, label) in [
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINE, "underline"),
        (Modifier::SLOW_BLINK, "slow-blink"),
        (Modifier::REVERSED, "reversed"),
        (Modifier::HIDDEN, "hidden"),
        (Modifier::CROSSED_OUT, "crossed-out"),
    ] {
        if style.modifiers.contains(modifier) {
            attributes.push(label.to_owned());
        }
    }
    attributes.join(",")
}

fn color_label(color: Color) -> String {
    match color {
        Color::Default => "default".to_owned(),
        Color::Black => "black".to_owned(),
        Color::Red => "red".to_owned(),
        Color::Green => "green".to_owned(),
        Color::Yellow => "yellow".to_owned(),
        Color::Blue => "blue".to_owned(),
        Color::Magenta => "magenta".to_owned(),
        Color::Cyan => "cyan".to_owned(),
        Color::White => "white".to_owned(),
        Color::BrightBlack => "bright-black".to_owned(),
        Color::BrightRed => "bright-red".to_owned(),
        Color::BrightGreen => "bright-green".to_owned(),
        Color::BrightYellow => "bright-yellow".to_owned(),
        Color::BrightBlue => "bright-blue".to_owned(),
        Color::BrightMagenta => "bright-magenta".to_owned(),
        Color::BrightCyan => "bright-cyan".to_owned(),
        Color::BrightWhite => "bright-white".to_owned(),
        Color::Indexed(index) => format!("indexed-{index}"),
        Color::Rgb(red, green, blue) => format!("rgb-{red}-{green}-{blue}"),
    }
}

#[test]
fn snapshots_all_heading_levels_at_width_40() {
    insta::assert_snapshot!("headings_width_40", styled_snapshot(HEADINGS, 40));
}

#[test]
fn snapshots_paragraph_breaks_and_rules_at_width_40() {
    insta::assert_snapshot!("paragraphs_width_40", visible_snapshot(PARAGRAPHS, 40));
}

#[test]
fn snapshots_horizontal_rule_at_width_3() {
    insta::assert_snapshot!("horizontal_rule_width_3", visible_snapshot("---", 3));
}

#[test]
fn snapshots_nested_and_wrapped_blockquotes_at_width_24() {
    insta::assert_snapshot!("blockquotes_width_24", visible_snapshot(BLOCKQUOTES, 24));
}

#[test]
fn snapshots_fenced_and_indented_code_at_width_40() {
    insta::assert_snapshot!("code_blocks_width_40", visible_snapshot(CODE_BLOCKS, 40));
}

#[test]
fn snapshots_blocks_at_width_40() {
    insta::assert_snapshot!("blocks_width_40", visible_snapshot(BLOCKS, 40));
}

#[test]
fn snapshots_composition_at_width_40() {
    insta::assert_snapshot!("composition_width_40", styled_snapshot(COMPOSITION, 40));
}

#[test]
fn snapshots_nested_inline_styles_at_width_50() {
    insta::assert_snapshot!(
        "inline_nested_styles_width_50",
        styled_snapshot(INLINE_NESTED, 50)
    );
}

#[test]
fn snapshots_formatted_text_wrapping_at_width_16() {
    insta::assert_snapshot!(
        "inline_wrapped_styles_width_16",
        styled_snapshot(
            "**strong text wraps** and [linked text wraps](https://example.com)",
            16
        )
    );
}

#[test]
fn snapshots_images_and_fallbacks_at_width_40() {
    insta::assert_snapshot!("images_width_40", visible_snapshot(IMAGES, 40));
}

#[test]
fn snapshots_html_and_xss_output_at_width_80() {
    insta::assert_snapshot!("html_xss_width_80", visible_snapshot(HTML_XSS, 80));
}

#[test]
fn dangerous_html_is_escaped_and_not_executably_rendered() {
    let output = visible_snapshot(HTML_XSS, 80);
    assert!(!output.contains("<script>"));
    assert!(output.contains("&amp;lt;script&amp;gt;"));
}

#[test]
fn snapshots_inline_visible_text_at_width_80() {
    insta::assert_snapshot!("inline_visible_width_80", visible_snapshot(INLINE, 80));
}

#[test]
fn snapshots_inline_styles_at_width_80() {
    insta::assert_snapshot!("inline_styles_width_80", styled_snapshot(INLINE, 80));
}

#[test]
fn snapshots_links_images_escapes_and_entities_at_width_40() {
    insta::assert_snapshot!("links_images_width_40", styled_snapshot(LINKS_IMAGES, 40));
}

#[test]
fn snapshots_tight_and_loose_lists_at_width_34() {
    insta::assert_snapshot!(
        "lists_tight_loose_width_34",
        visible_snapshot(LISTS_TIGHT_LOOSE, 34)
    );
}

#[test]
fn snapshots_numbering_and_mixed_nested_lists_at_width_28() {
    insta::assert_snapshot!(
        "lists_numbering_width_28",
        visible_snapshot(LISTS_NUMBERING, 28)
    );
}

#[test]
fn snapshots_lists_at_width_30() {
    insta::assert_snapshot!("lists_width_30", styled_snapshot(LISTS, 30));
}

#[test]
fn snapshots_partial_streaming_inputs_at_width_40() {
    let snapshot = [
        "== code ==",
        &visible_snapshot(STREAMING_CODE, 40),
        "== inline ==",
        &visible_snapshot(STREAMING_INLINE, 40),
        "== structures ==",
        &visible_snapshot(STREAMING_STRUCTURES, 40),
    ]
    .join("\n");
    insta::assert_snapshot!("streaming_partial_width_40", snapshot);
}

#[test]
fn requested_widths_80_40_and_20_are_snapshot_covered() {
    insta::assert_snapshot!(
        "representative_widths_80_40_20",
        [
            "== width 80 ==",
            &visible_snapshot(COMPOSITION, 80),
            "== width 40 ==",
            &visible_snapshot(COMPOSITION, 40),
            "== width 20 ==",
            &visible_snapshot(COMPOSITION, 20),
        ]
        .join("\n")
    );
}

#[test]
fn snapshots_unicode_at_width_20() {
    insta::assert_snapshot!("unicode_width_20", visible_snapshot(UNICODE, 20));
}

#[test]
fn snapshots_table_with_one_body_row_at_width_80() {
    insta::assert_snapshot!(
        "table_single_row_width_80",
        styled_snapshot(TABLE_SINGLE_ROW, 80)
    );
}

#[test]
fn snapshots_table_that_exactly_fits_width_17() {
    insta::assert_snapshot!(
        "table_exact_fit_width_17",
        visible_snapshot(TABLE_MULTIPLE_ROWS, 17)
    );
}

#[test]
fn snapshots_table_with_multiple_body_rows_at_width_80() {
    insta::assert_snapshot!(
        "table_multiple_rows_width_80",
        visible_snapshot(TABLE_MULTIPLE_ROWS, 80)
    );
}

#[test]
fn snapshots_table_with_empty_and_uneven_cells_at_width_80() {
    insta::assert_snapshot!("table_uneven_width_80", visible_snapshot(TABLE_UNEVEN, 80));
}

#[test]
fn snapshots_formatted_table_styles_at_width_80() {
    insta::assert_snapshot!(
        "table_formatted_styles_width_80",
        styled_snapshot(TABLE_FORMATTED, 80)
    );
}

#[test]
fn snapshots_narrow_table_fallback_at_width_12() {
    insta::assert_snapshot!(
        "table_narrow_fallback_width_12",
        visible_snapshot(TABLE_MULTIPLE_ROWS, 12)
    );
}

#[test]
fn snapshots_unicode_table_at_width_80() {
    insta::assert_snapshot!(
        "table_unicode_width_80",
        visible_snapshot(TABLE_UNICODE, 80)
    );
}

#[test]
fn rendered_lines_respect_requested_width_for_wrappable_text() {
    for width in [2_u16, 20, 40, 80] {
        for line in render(UNICODE, width) {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= usize::from(width.max(1)),
                "line exceeds width {width}: {text:?}"
            );
        }
    }
}

#[test]
fn defensive_widths_do_not_panic_for_supported_structures() {
    for width in [0_u16, 1] {
        for markdown in [BLOCKS, INLINE, LISTS, TABLE_MULTIPLE_ROWS, UNICODE] {
            let _ = render(markdown, width);
        }
    }
}

#[test]
fn zero_width_is_treated_as_one_without_panicking() {
    let lines = render("abc", 0);
    assert_eq!(lines.len(), 3);
}

#[test]
fn bordered_table_lines_have_consistent_display_width() {
    let lines = render(TABLE_MULTIPLE_ROWS, 80);
    let widths = lines
        .iter()
        .map(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            UnicodeWidthStr::width(text.as_str())
        })
        .collect::<Vec<_>>();

    assert!(!widths.is_empty());
    assert!(widths.iter().all(|width| *width == widths[0]));
}

#[test]
fn bordered_table_content_has_matching_vertical_edges() {
    for line in render(TABLE_MULTIPLE_ROWS, 80) {
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        if text.starts_with('│') {
            assert!(text.ends_with('│'), "table row lacks right edge: {text:?}");
        }
    }
}
