//! Native TUI rendering for filesystem file-change previews.

use bcode_syntax_render::SyntaxStyle;
use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::file_change_diff::{
    ChangedRange, FileChangeDiffLine, FileChangeDiffLineKind, diff_from_text,
};

const MAX_INLINE_DIFF_ROWS: usize = 24;
const INLINE_DIFF_CARD_MIN_WIDTH: usize = 24;
const INLINE_DIFF_CARD_CHROME_WIDTH: usize = 14;
const INLINE_DIFF_BODY_CHROME_WIDTH: usize = 14;

#[derive(Debug, Clone, Copy)]
enum PreviewRow<'a> {
    Line(&'a FileChangeDiffLine),
    Hidden(usize),
}

pub struct FileChangeTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for FileChangeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        // Keep the old plugin-view schema as a local TUI-only replay shim for pre-artifact logs.
        matches!(
            kind,
            "bcode.filesystem.change" | "bcode.filesystem.file_change"
        )
    }

    fn rows(&self, _kind: &str, payload: &serde_json::Value, width: u16) -> Vec<Line> {
        let path = payload
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<path>");
        let old_text = payload
            .get("old_text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let new_text = payload
            .get("new_text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let title = payload
            .get("title")
            .and_then(serde_json::Value::as_str)
            .or_else(|| payload.get("summary").and_then(serde_json::Value::as_str))
            .unwrap_or_else(|| {
                if payload.get("tool_name").is_some() {
                    "File change"
                } else {
                    "Streaming preview"
                }
            });
        let subtitle = payload.get("subtitle").and_then(serde_json::Value::as_str);
        let argument_bytes = payload
            .get("argument_bytes")
            .and_then(serde_json::Value::as_u64)
            .and_then(|bytes| usize::try_from(bytes).ok());
        let truncated = payload
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        file_change_rows(
            FileChangeRowsInput {
                path,
                old_text,
                new_text,
                title,
                subtitle,
                argument_bytes,
                truncated,
            },
            width,
        )
    }
}

fn format_preview_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes >= MIB {
        let whole = bytes / MIB;
        let decimal = (bytes % MIB) * 10 / MIB;
        format!("{whole}.{decimal} MiB")
    } else if bytes >= KIB {
        let whole = bytes / KIB;
        let decimal = (bytes % KIB) * 10 / KIB;
        format!("{whole}.{decimal} KiB")
    } else {
        format!("{bytes} B")
    }
}

#[derive(Debug, Clone, Copy)]
struct FileChangeRowsInput<'a> {
    path: &'a str,
    old_text: &'a str,
    new_text: &'a str,
    title: &'a str,
    subtitle: Option<&'a str>,
    argument_bytes: Option<usize>,
    truncated: bool,
}

fn file_change_rows(input: FileChangeRowsInput<'_>, width: u16) -> Vec<Line> {
    let diff = diff_from_text(input.path, input.old_text, input.new_text);
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(
            format!(
                "{} · {}",
                input.title,
                mode_label(input.old_text.is_empty())
            ),
            Style::new().fg(Color::Cyan),
        ),
    ]));
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(
            format!("{}  +{} -{}", diff.path, diff.added, diff.removed),
            Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(change_summary(diff.added, diff.removed), muted_style()),
    ]));

    if let Some(argument_bytes) = input.argument_bytes {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                format!("received: {}", format_preview_bytes(argument_bytes)),
                muted_style(),
            ),
        ]));
    }
    if let Some(subtitle) = input.subtitle {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(subtitle.to_owned(), muted_style()),
        ]));
    }

    if input.truncated {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                "preview truncated; showing available diff rows",
                muted_style(),
            ),
        ]));
    }

    let visible_lines = diff
        .lines
        .iter()
        .filter(|line| is_preview_content_line(line.kind))
        .cloned()
        .collect::<Vec<_>>();
    if visible_lines.is_empty() {
        return rows;
    }

    let total_rows = visible_lines.len();
    let shown_rows = total_rows.min(MAX_INLINE_DIFF_ROWS);
    let progress = if total_rows > shown_rows {
        format!(
            "live preview · showing {shown_rows} of {total_rows} diff rows · /diff for full view"
        )
    } else {
        "live preview · /diff for full view".to_owned()
    };
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(progress, muted_style()),
    ]));

    let preview = inline_preview(&visible_lines, MAX_INLINE_DIFF_ROWS);
    let card_width = card_width(&preview, width.saturating_sub(2));
    rows.push(card_border('┌', '─', '┐', card_width));
    for row in preview {
        match row {
            PreviewRow::Line(line) => rows.extend(render_diff_line(line, card_width)),
            PreviewRow::Hidden(count) => rows.push(hidden_row(count, card_width)),
        }
    }
    rows.push(card_border('└', '─', '┘', card_width));
    rows
}

const fn is_preview_content_line(kind: FileChangeDiffLineKind) -> bool {
    matches!(
        kind,
        FileChangeDiffLineKind::Added
            | FileChangeDiffLineKind::Removed
            | FileChangeDiffLineKind::Context
    )
}

fn inline_preview(lines: &[FileChangeDiffLine], max_rows: usize) -> Vec<PreviewRow<'_>> {
    if lines.len() <= max_rows || max_rows < 4 {
        return lines.iter().map(PreviewRow::Line).collect();
    }
    let head = max_rows / 2;
    let tail = max_rows.saturating_sub(head).saturating_sub(1);
    let hidden = lines.len().saturating_sub(head).saturating_sub(tail);
    lines
        .iter()
        .take(head)
        .map(PreviewRow::Line)
        .chain(std::iter::once(PreviewRow::Hidden(hidden)))
        .chain(
            lines
                .iter()
                .skip(lines.len().saturating_sub(tail))
                .map(PreviewRow::Line),
        )
        .collect()
}

fn render_diff_line(line: &FileChangeDiffLine, width: u16) -> Vec<Line> {
    let (sign, sign_style, body_style) = line_styles(line.kind);
    let row_style = row_style(line.kind);
    let emphasis_style = emphasis_style(line.kind);
    let gutter_style = row_style.patch(muted_style());
    let body_width = usize::from(width)
        .saturating_sub(INLINE_DIFF_BODY_CHROME_WIDTH)
        .max(1);
    let chunks = wrap_spans(
        content_spans(line, row_style.patch(body_style)),
        &line.changed_ranges,
        emphasis_style,
        body_width,
    );
    let chunks = if chunks.is_empty() {
        vec![vec![Span::styled(
            String::new(),
            row_style.patch(body_style),
        )]]
    } else {
        chunks
    };
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let mut spans = vec![Span::styled("  ", muted_style())];
            let mut card_spans = if index == 0 {
                vec![
                    Span::styled("│ ", muted_style()),
                    Span::styled("  ", gutter_style),
                    Span::styled(
                        sign,
                        row_style.patch(sign_style.add_modifier(Modifier::BOLD)),
                    ),
                    Span::styled(format!("{:>4}", line_number(line)), gutter_style),
                    Span::styled(" │ ", gutter_style),
                ]
            } else {
                continuation_prefix(gutter_style)
            };
            card_spans.extend(chunk);
            pad_card_spans(
                &mut card_spans,
                usize::from(width).saturating_sub(2),
                row_style,
            );
            card_spans.push(Span::styled(" │", muted_style()));
            spans.extend(card_spans);
            Line::from_spans(spans)
        })
        .collect()
}

fn continuation_prefix(gutter_style: Style) -> Vec<Span> {
    vec![
        Span::styled("│ ", muted_style()),
        Span::styled("  ", gutter_style),
        Span::styled(" ", gutter_style),
        Span::styled("    ", gutter_style),
        Span::styled(" │ ", gutter_style),
    ]
}

fn pad_card_spans(spans: &mut Vec<Span>, target_width: usize, style: Style) {
    let current_width = spans_width(spans);
    if current_width < target_width {
        spans.push(Span::styled(
            " ".repeat(target_width - current_width),
            style,
        ));
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_str()))
        .sum()
}

fn content_spans(line: &FileChangeDiffLine, fallback_style: Style) -> Vec<Span> {
    if line.syntax_spans.is_empty() {
        return vec![Span::styled(line.content.clone(), fallback_style)];
    }
    line.syntax_spans
        .iter()
        .map(|span| {
            Span::styled(
                span.content.clone(),
                fallback_style.patch(syntax_style(span.style)),
            )
        })
        .collect()
}

fn wrap_spans(
    spans: Vec<Span>,
    changed_ranges: &[ChangedRange],
    emphasis_style: Style,
    width: usize,
) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut rows = Vec::<Vec<Span>>::new();
    let mut current = Vec::<Span>::new();
    let mut current_width = 0usize;
    let mut byte_offset = 0usize;
    for span in spans {
        let text = span.content.clone();
        for grapheme in text.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let start = byte_offset;
            let end = byte_offset.saturating_add(grapheme.len());
            byte_offset = end;
            let mut style = span.style;
            if changed_ranges
                .iter()
                .any(|range| ranges_overlap(start, end, range.start, range.end))
            {
                style = style.patch(emphasis_style);
            }
            current.push(Span::styled(grapheme.to_owned(), style));
            current_width = current_width.saturating_add(grapheme_width);
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

const fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start < b_end && b_start < a_end
}

fn card_width(preview: &[PreviewRow<'_>], available_width: u16) -> u16 {
    let available = usize::from(available_width.max(1));
    let content_width = preview
        .iter()
        .map(|row| match row {
            PreviewRow::Line(line) => UnicodeWidthStr::width(line.content.as_str()),
            PreviewRow::Hidden(count) => UnicodeWidthStr::width(hidden_text(*count).as_str()),
        })
        .max()
        .unwrap_or(0);
    u16::try_from(
        content_width
            .saturating_add(INLINE_DIFF_CARD_CHROME_WIDTH)
            .clamp(INLINE_DIFF_CARD_MIN_WIDTH.min(available), available),
    )
    .unwrap_or(u16::MAX)
}

fn card_border(left: char, fill: char, right: char, width: u16) -> Line {
    let inner_width = usize::from(width.saturating_sub(2));
    Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(
            format!("{left}{}{right}", fill.to_string().repeat(inner_width)),
            muted_style(),
        ),
    ])
}

fn hidden_row(count: usize, width: u16) -> Line {
    let text = hidden_text(count);
    let inner_width = usize::from(width.saturating_sub(4));
    let clipped = truncate_to_display_width(&text, inner_width);
    let clipped_width = UnicodeWidthStr::width(clipped.as_str());
    let mut spans = vec![
        Span::styled("  ", muted_style()),
        Span::styled("│ ", muted_style()),
        Span::styled(clipped, muted_style()),
    ];
    let padding = inner_width.saturating_sub(clipped_width);
    if padding > 0 {
        spans.push(Span::styled(" ".repeat(padding), muted_style()));
    }
    spans.push(Span::styled(" │", muted_style()));
    Line::from_spans(spans)
}

fn truncate_to_display_width(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut output_width = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if output_width.saturating_add(grapheme_width) > width {
            break;
        }
        output.push_str(grapheme);
        output_width = output_width.saturating_add(grapheme_width);
    }
    output
}

fn hidden_text(count: usize) -> String {
    format!("… {count} diff rows hidden …")
}

const fn mode_label(old_text_is_empty: bool) -> &'static str {
    if old_text_is_empty {
        "Writing file"
    } else {
        "Editing file"
    }
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

fn line_number(line: &FileChangeDiffLine) -> String {
    line.new_line
        .or(line.old_line)
        .map_or_else(|| "·".to_owned(), |line| line.to_string())
}

fn change_summary(added: u32, removed: u32) -> String {
    match (added, removed) {
        (0, 0) => "no textual changes detected".to_owned(),
        (added, 0) => format!("added {}", line_count_label(added)),
        (0, removed) => format!("removed {}", line_count_label(removed)),
        (added, removed) => format!(
            "replaced {} with {}",
            line_count_label(removed),
            line_count_label(added)
        ),
    }
}

fn line_count_label(count: u32) -> String {
    if count == 1 {
        "1 line".to_owned()
    } else {
        format!("{count} lines")
    }
}

const fn row_style(kind: FileChangeDiffLineKind) -> Style {
    match kind {
        FileChangeDiffLineKind::Added => Style::new().bg(Color::Rgb(0, 24, 16)),
        FileChangeDiffLineKind::Removed => Style::new().bg(Color::Rgb(32, 10, 10)),
        FileChangeDiffLineKind::Context
        | FileChangeDiffLineKind::HunkHeader
        | FileChangeDiffLineKind::FileHeader => Style::new(),
    }
}

const fn emphasis_style(kind: FileChangeDiffLineKind) -> Style {
    match kind {
        FileChangeDiffLineKind::Added => Style::new().bg(Color::Rgb(0, 42, 26)),
        FileChangeDiffLineKind::Removed => Style::new().bg(Color::Rgb(50, 14, 14)),
        FileChangeDiffLineKind::Context
        | FileChangeDiffLineKind::HunkHeader
        | FileChangeDiffLineKind::FileHeader => Style::new(),
    }
}

const fn line_styles(kind: FileChangeDiffLineKind) -> (&'static str, Style, Style) {
    match kind {
        FileChangeDiffLineKind::Added => (
            "+",
            Style::new().fg(Color::BrightGreen),
            Style::new().fg(Color::BrightGreen),
        ),
        FileChangeDiffLineKind::Removed => (
            "-",
            Style::new().fg(Color::BrightRed),
            Style::new().fg(Color::BrightRed),
        ),
        FileChangeDiffLineKind::HunkHeader => (
            "·",
            Style::new().fg(Color::BrightCyan),
            Style::new().fg(Color::BrightCyan),
        ),
        FileChangeDiffLineKind::FileHeader => ("·", muted_style(), muted_style()),
        FileChangeDiffLineKind::Context => (" ", muted_style(), Style::new()),
    }
}

const fn syntax_style(style: SyntaxStyle) -> Style {
    let mut tui_style = Style::new().fg(Color::Rgb(
        style.foreground_r,
        style.foreground_g,
        style.foreground_b,
    ));
    if style.bold {
        tui_style = tui_style.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        tui_style = tui_style.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        tui_style = tui_style.add_modifier(Modifier::UNDERLINE);
    }
    tui_style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_file_change_rows(
        path: &str,
        old_text: &str,
        new_text: &str,
        title: &str,
        truncated: bool,
        width: u16,
    ) -> Vec<Line> {
        file_change_rows(
            FileChangeRowsInput {
                path,
                old_text,
                new_text,
                title,
                subtitle: None,
                argument_bytes: None,
                truncated,
            },
            width,
        )
    }

    #[test]
    fn adapter_supports_raw_filesystem_change_artifact_schema() {
        let payload = serde_json::json!({
            "path": "src/lib.rs",
            "summary": "edited file",
            "old_text": "before\n",
            "new_text": "after\n"
        });
        assert!(bcode_plugin_sdk::tui::PluginTuiVisualAdapter::supports(
            &FileChangeTuiVisualAdapter,
            "bcode.filesystem.change"
        ));

        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &FileChangeTuiVisualAdapter,
            "bcode.filesystem.change",
            &payload,
            80,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("src/lib.rs"), "{rendered}");
        assert!(rendered.contains("before"), "{rendered}");
        assert!(rendered.contains("after"), "{rendered}");
    }

    #[test]
    fn file_change_rows_show_counts_and_hidden_rows() {
        let new_text = (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = test_file_change_rows("src/lib.rs", "", &new_text, "Applying", false, 80);
        let rendered = format!("{rows:?}");
        assert!(rendered.contains("src/lib.rs  +40 -0"));
        assert!(rendered.contains("diff rows hidden"));
    }

    #[test]
    fn file_change_rows_use_pre_migration_inline_diff_palette() {
        let rows = test_file_change_rows(
            "src/lib.rs",
            "let value = 1;\n",
            "let value = 2;\n",
            "File change",
            false,
            80,
        );
        let rendered = format!("{rows:?}");
        assert!(rendered.contains("Rgb(0, 24, 16)"), "{rendered}");
        assert!(rendered.contains("Rgb(32, 10, 10)"), "{rendered}");
        assert!(rendered.contains("Rgb(0, 42, 26)"), "{rendered}");
        assert!(rendered.contains("Rgb(50, 14, 14)"), "{rendered}");
        assert!(!rendered.contains("Indexed(22)"), "{rendered}");
        assert!(!rendered.contains("Indexed(52)"), "{rendered}");
        assert!(!rendered.contains("UNDERLINE"), "{rendered}");
    }

    #[test]
    fn file_change_rows_render_available_new_text_without_pending_warning() {
        let rows = test_file_change_rows(
            "src/lib.rs",
            "",
            "fn main() {\n    println!(\"hello\");\n}\n",
            "Editing",
            false,
            80,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("println!"), "{rendered}");
        assert!(!rendered.contains("original text pending"), "{rendered}");
        assert!(!rendered.contains("waiting for original"), "{rendered}");
        assert!(
            !rendered.contains("showing available new text"),
            "{rendered}"
        );
    }

    #[test]
    fn adapter_ignores_legacy_original_pending_payload_warning() {
        let payload = serde_json::json!({
            "path": "src/lib.rs",
            "old_text": "",
            "new_text": "fn main() {}\n",
            "original_pending": true
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &FileChangeTuiVisualAdapter,
            "bcode.filesystem.file_change",
            &payload,
            80,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("fn main"), "{rendered}");
        assert!(!rendered.contains("original text pending"), "{rendered}");
        assert!(
            !rendered.contains("showing available new text"),
            "{rendered}"
        );
    }

    #[test]
    fn file_change_rows_use_pre_migration_progress_and_line_number_fallback() {
        let rows = test_file_change_rows(
            "src/lib.rs",
            "let value = 1;\n",
            "let value = 2;\n",
            "File change",
            false,
            80,
        );
        let rendered = format!("{rows:?}");
        assert!(
            rendered.contains("live preview · /diff for full view"),
            "{rendered}"
        );

        let line = FileChangeDiffLine::new(FileChangeDiffLineKind::Context, None, None, "context");
        let rendered = format!("{:?}", render_diff_line(&line, 80));
        assert!(rendered.contains("   ·"), "{rendered}");
    }

    #[test]
    fn hidden_rows_pad_to_card_width() {
        let width = 32;
        let row = hidden_row(7, width);
        let rendered_width: usize = row
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref() as &str))
            .sum();
        assert_eq!(rendered_width, usize::from(width) + 2);
    }

    #[test]
    fn fitting_diff_lines_do_not_wrap() {
        let content = "01234567890123456789012345678901234567890123456789";
        let line = FileChangeDiffLine::new(FileChangeDiffLineKind::Added, None, Some(1), content);
        let card_width = u16::try_from(content.len() + INLINE_DIFF_BODY_CHROME_WIDTH)
            .expect("card width fits u16");

        let rows = render_diff_line(&line, card_width);

        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(line_width(&rows[0]), usize::from(card_width) + 2);
    }

    #[test]
    fn wrapped_diff_lines_keep_a_straight_right_edge() {
        let line = FileChangeDiffLine::new(
            FileChangeDiffLineKind::Added,
            None,
            Some(1),
            "0123456789012345678901234567890123456789",
        );
        let card_width = 28;

        let rows = render_diff_line(&line, card_width);

        assert!(rows.len() > 1, "{rows:?}");
        for row in rows {
            assert_eq!(line_width(&row), usize::from(card_width) + 2, "{row:?}");
        }
    }

    #[test]
    fn file_change_card_excludes_file_and_hunk_headers() {
        let rows = test_file_change_rows(
            "src/lib.rs",
            "fn main() {\n    let value = 1;\n}\n",
            "fn main() {\n    let value = 2;\n}\n",
            "File change",
            false,
            80,
        );
        let rendered_card = rows
            .iter()
            .filter(|row| is_card_row(row))
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!rendered_card.contains("--- "), "{rendered_card}");
        assert!(!rendered_card.contains("+++ "), "{rendered_card}");
        assert!(!rendered_card.contains("@@"), "{rendered_card}");
        assert!(rendered_card.contains("let value = 1"), "{rendered_card}");
        assert!(rendered_card.contains("let value = 2"), "{rendered_card}");
    }

    #[test]
    fn progress_count_excludes_file_and_hunk_headers() {
        let rows = test_file_change_rows(
            "src/lib.rs",
            "fn main() {\n    let value = 1;\n}\n",
            "fn main() {\n    let value = 2;\n}\n",
            "File change",
            false,
            80,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(
            rendered.contains("live preview · /diff for full view"),
            "{rendered}"
        );
        assert!(!rendered.contains("showing 5 of"), "{rendered}");
    }

    #[test]
    fn no_content_diff_rows_skip_the_card() {
        let rows = test_file_change_rows("src/lib.rs", "", "", "File change", false, 80);
        assert!(!rows.iter().any(is_card_row));
    }

    #[test]
    fn file_change_card_rows_share_one_width_and_never_exceed_available_width() {
        let available_width = 80;
        let rows = test_file_change_rows(
            "src/lib.rs",
            "let value = 1;\n",
            "let value = 2;\n",
            "File change",
            false,
            available_width,
        );
        let card_widths = rows
            .iter()
            .filter(|row| is_card_row(row))
            .map(line_width)
            .collect::<Vec<_>>();

        assert!(!card_widths.is_empty());
        let first_width = card_widths[0];
        for width in card_widths {
            assert_eq!(width, first_width);
            assert!(width <= usize::from(available_width));
        }
    }

    #[test]
    fn file_change_rows_include_headers() {
        let rows = test_file_change_rows(
            "src/lib.rs",
            "let x = 1;\n",
            "let x = 2;\n",
            "File change",
            false,
            80,
        );
        let rendered = format!("{rows:?}");
        assert!(rendered.contains("src/lib.rs"));
    }

    fn line_width(line: &Line) -> usize {
        line.spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref() as &str))
            .sum()
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref() as &str)
            .collect::<String>()
    }

    fn is_card_row(line: &Line) -> bool {
        line.spans
            .get(1)
            .is_some_and(|span| span.content.starts_with(['┌', '│', '└']))
    }
}
