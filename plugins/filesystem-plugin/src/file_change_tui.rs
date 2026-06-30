//! Native TUI rendering for filesystem file-change previews.

use bcode_syntax_render::SyntaxStyle;
use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::file_change_diff::{
    ChangedRange, FileChangeDiffLine, FileChangeDiffLineKind, diff_from_text,
};

const MAX_INLINE_DIFF_ROWS: usize = 24;
const INLINE_DIFF_CARD_MIN_WIDTH: usize = 24;
const INLINE_DIFF_CARD_CHROME_WIDTH: usize = 10;
const INLINE_DIFF_BODY_CHROME_WIDTH: usize = 11;

#[derive(Debug, Clone, Copy)]
enum PreviewRow<'a> {
    Line(&'a FileChangeDiffLine),
    Hidden(usize),
}

pub struct FileChangeTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for FileChangeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        kind == "bcode.filesystem.file_change"
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
        let subtitle = payload.get("subtitle").and_then(serde_json::Value::as_str);
        let truncated = payload
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let original_pending = payload
            .get("original_pending")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(old_text.is_empty());
        file_change_rows(
            path,
            old_text,
            new_text,
            subtitle,
            truncated,
            original_pending,
            width,
        )
    }
}

fn file_change_rows(
    path: &str,
    old_text: &str,
    new_text: &str,
    subtitle: Option<&str>,
    truncated: bool,
    original_pending: bool,
    width: u16,
) -> Vec<Line> {
    let diff = diff_from_text(path, old_text, new_text);
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(
            format!(
                "{} · {}",
                subtitle.unwrap_or("Streaming preview"),
                mode_label(old_text.is_empty())
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

    if original_pending {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                "original text pending; showing available new text",
                muted_style(),
            ),
        ]));
    }
    if truncated {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                "preview truncated; showing available diff rows",
                muted_style(),
            ),
        ]));
    }

    let total_rows = diff.lines.len();
    let shown_rows = total_rows.min(MAX_INLINE_DIFF_ROWS);
    let progress = if total_rows > shown_rows {
        format!("live preview · showing {shown_rows} of {total_rows} diff rows")
    } else {
        "live preview".to_owned()
    };
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(progress, muted_style()),
    ]));

    let preview = inline_preview(&diff.lines, MAX_INLINE_DIFF_ROWS);
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
    let gutter_style = row_style.patch(muted_style());
    let body_width = usize::from(width)
        .saturating_sub(INLINE_DIFF_BODY_CHROME_WIDTH)
        .max(1);
    let chunks = wrap_spans(
        content_spans(line, row_style.patch(body_style)),
        &line.changed_ranges,
        row_style,
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
            spans.extend([
                Span::styled("│ ", muted_style()),
                Span::styled("  ", gutter_style),
                Span::styled(
                    if index == 0 { sign } else { " " },
                    row_style.patch(sign_style.add_modifier(Modifier::BOLD)),
                ),
                Span::styled(
                    if index == 0 {
                        format!("{:>4}", line_number(line))
                    } else {
                        "    ".to_owned()
                    },
                    gutter_style,
                ),
                Span::styled(" │ ", gutter_style),
            ]);
            spans.extend(chunk);
            Line::from_spans(spans)
        })
        .collect()
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
    row_style: Style,
    width: usize,
) -> Vec<Vec<Span>> {
    let mut rows = Vec::<Vec<Span>>::new();
    let mut current = Vec::<Span>::new();
    let mut current_width = 0usize;
    let mut byte_offset = 0usize;
    for span in spans {
        let text = span.content.clone();
        for grapheme in text.graphemes(true) {
            if current_width >= width && !current.is_empty() {
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
                style = row_style.patch(
                    style
                        .add_modifier(Modifier::BOLD)
                        .add_modifier(Modifier::UNDERLINE),
                );
            }
            current.push(Span::styled(grapheme.to_owned(), style));
            current_width = current_width.saturating_add(1);
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
            PreviewRow::Line(line) => line.content.graphemes(true).count(),
            PreviewRow::Hidden(count) => hidden_text(*count).len(),
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
    let clipped = text.chars().take(inner_width).collect::<String>();
    Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled("│ ", muted_style()),
        Span::styled(clipped, muted_style()),
        Span::styled(" │", muted_style()),
    ])
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
        .map_or_else(String::new, |line| line.to_string())
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
        FileChangeDiffLineKind::Added => Style::new().bg(Color::Indexed(22)),
        FileChangeDiffLineKind::Removed => Style::new().bg(Color::Indexed(52)),
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

    #[test]
    fn file_change_rows_show_counts_and_hidden_rows() {
        let new_text = (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = file_change_rows(
            "src/lib.rs",
            "",
            &new_text,
            Some("Applying"),
            false,
            true,
            80,
        );
        let rendered = format!("{rows:?}");
        assert!(rendered.contains("src/lib.rs  +40 -0"));
        assert!(rendered.contains("diff rows hidden"));
    }

    #[test]
    fn file_change_rows_include_headers() {
        let rows = file_change_rows(
            "src/lib.rs",
            "let x = 1;\n",
            "let x = 2;\n",
            None,
            false,
            false,
            80,
        );
        let rendered = format!("{rows:?}");
        assert!(rendered.contains("src/lib.rs"));
    }
}
