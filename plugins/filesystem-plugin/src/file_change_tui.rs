//! Native TUI rendering for filesystem file-change previews.

use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use unicode_segmentation::UnicodeSegmentation;

const MAX_INLINE_DIFF_ROWS: usize = 24;
const DIFF_CONTEXT_LINES: usize = 3;
const MAX_LCS_CELLS: usize = 40_000;
const INLINE_DIFF_CARD_MIN_WIDTH: usize = 24;
const INLINE_DIFF_CARD_CHROME_WIDTH: usize = 10;
const INLINE_DIFF_BODY_CHROME_WIDTH: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    Context,
    Added,
    Removed,
    HunkHeader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffLine {
    kind: DiffLineKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
    content: String,
}

impl DiffLine {
    fn new(
        kind: DiffLineKind,
        old_line: Option<u32>,
        new_line: Option<u32>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            old_line,
            new_line,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PreviewRow<'a> {
    Line(&'a DiffLine),
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
        file_change_rows(path, old_text, new_text, subtitle, width)
    }
}

fn file_change_rows(
    path: &str,
    old_text: &str,
    new_text: &str,
    subtitle: Option<&str>,
    width: u16,
) -> Vec<Line> {
    let mut rows = Vec::new();
    let diff_lines = diff_lines(old_text, new_text);
    let (added, removed) = changed_counts(&diff_lines);
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(
            format!(
                "{} · {}",
                subtitle.unwrap_or("File change"),
                mode_label(old_text.is_empty())
            ),
            Style::new().fg(Color::Cyan),
        ),
    ]));
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(
            format!("{path}  +{added} -{removed}"),
            Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    rows.push(Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(change_summary(added, removed), muted_style()),
    ]));

    if old_text.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                "original text pending; showing available new text",
                muted_style(),
            ),
        ]));
    }

    let total_rows = diff_lines.len();
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

    let preview = inline_preview(&diff_lines, MAX_INLINE_DIFF_ROWS);
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

fn diff_lines(old_text: &str, new_text: &str) -> Vec<DiffLine> {
    let old_lines = old_text.lines().collect::<Vec<_>>();
    let new_lines = new_text.lines().collect::<Vec<_>>();
    if old_lines == new_lines {
        return old_lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let number = usize_to_u32(index.saturating_add(1));
                DiffLine::new(DiffLineKind::Context, Some(number), Some(number), *line)
            })
            .collect();
    }
    let prefix = common_prefix_len(&old_lines, &new_lines);
    let suffix = common_suffix_len(&old_lines, &new_lines, prefix);
    let old_change_end = old_lines.len().saturating_sub(suffix);
    let new_change_end = new_lines.len().saturating_sub(suffix);
    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let old_context_end = old_change_end
        .saturating_add(DIFF_CONTEXT_LINES)
        .min(old_lines.len());
    let new_context_end = new_change_end
        .saturating_add(DIFF_CONTEXT_LINES)
        .min(new_lines.len());
    let mut lines = vec![DiffLine::new(
        DiffLineKind::HunkHeader,
        None,
        None,
        hunk_header(context_start, old_context_end, new_context_end),
    )];
    for (index, line) in old_lines
        .iter()
        .enumerate()
        .take(prefix)
        .skip(context_start)
    {
        let number = usize_to_u32(index.saturating_add(1));
        lines.push(DiffLine::new(
            DiffLineKind::Context,
            Some(number),
            Some(number),
            *line,
        ));
    }
    push_changed_lines(
        &mut lines,
        &old_lines[prefix..old_change_end],
        &new_lines[prefix..new_change_end],
        prefix,
    );
    let context_count = old_context_end
        .saturating_sub(old_change_end)
        .min(new_context_end.saturating_sub(new_change_end));
    for offset in 0..context_count {
        let old_index = old_change_end.saturating_add(offset);
        let new_index = new_change_end.saturating_add(offset);
        lines.push(DiffLine::new(
            DiffLineKind::Context,
            Some(usize_to_u32(old_index.saturating_add(1))),
            Some(usize_to_u32(new_index.saturating_add(1))),
            old_lines[old_index],
        ));
    }
    lines
}

fn push_changed_lines(
    lines: &mut Vec<DiffLine>,
    old_lines: &[&str],
    new_lines: &[&str],
    base: usize,
) {
    if old_lines.len().saturating_mul(new_lines.len()) > MAX_LCS_CELLS {
        push_removed(lines, old_lines, base);
        push_added(lines, new_lines, base);
        return;
    }
    let table = lcs_table(old_lines, new_lines);
    let (mut old_index, mut new_index) = (0usize, 0usize);
    while old_index < old_lines.len() && new_index < new_lines.len() {
        if old_lines[old_index] == new_lines[new_index] {
            let number = usize_to_u32(base.saturating_add(old_index).saturating_add(1));
            lines.push(DiffLine::new(
                DiffLineKind::Context,
                Some(number),
                Some(usize_to_u32(
                    base.saturating_add(new_index).saturating_add(1),
                )),
                old_lines[old_index],
            ));
            old_index = old_index.saturating_add(1);
            new_index = new_index.saturating_add(1);
        } else if table[old_index.saturating_add(1)][new_index]
            >= table[old_index][new_index.saturating_add(1)]
        {
            lines.push(DiffLine::new(
                DiffLineKind::Removed,
                Some(usize_to_u32(
                    base.saturating_add(old_index).saturating_add(1),
                )),
                None,
                old_lines[old_index],
            ));
            old_index = old_index.saturating_add(1);
        } else {
            lines.push(DiffLine::new(
                DiffLineKind::Added,
                None,
                Some(usize_to_u32(
                    base.saturating_add(new_index).saturating_add(1),
                )),
                new_lines[new_index],
            ));
            new_index = new_index.saturating_add(1);
        }
    }
    push_removed(
        lines,
        &old_lines[old_index..],
        base.saturating_add(old_index),
    );
    push_added(
        lines,
        &new_lines[new_index..],
        base.saturating_add(new_index),
    );
}

fn push_removed(lines: &mut Vec<DiffLine>, removed: &[&str], base: usize) {
    for (index, line) in removed.iter().enumerate() {
        lines.push(DiffLine::new(
            DiffLineKind::Removed,
            Some(usize_to_u32(base.saturating_add(index).saturating_add(1))),
            None,
            *line,
        ));
    }
}

fn push_added(lines: &mut Vec<DiffLine>, added: &[&str], base: usize) {
    for (index, line) in added.iter().enumerate() {
        lines.push(DiffLine::new(
            DiffLineKind::Added,
            None,
            Some(usize_to_u32(base.saturating_add(index).saturating_add(1))),
            *line,
        ));
    }
}

fn lcs_table(old_lines: &[&str], new_lines: &[&str]) -> Vec<Vec<usize>> {
    let mut table =
        vec![vec![0usize; new_lines.len().saturating_add(1)]; old_lines.len().saturating_add(1)];
    for old_index in (0..old_lines.len()).rev() {
        for new_index in (0..new_lines.len()).rev() {
            table[old_index][new_index] = if old_lines[old_index] == new_lines[new_index] {
                table[old_index.saturating_add(1)][new_index.saturating_add(1)].saturating_add(1)
            } else {
                table[old_index.saturating_add(1)][new_index]
                    .max(table[old_index][new_index.saturating_add(1)])
            };
        }
    }
    table
}

fn inline_preview(lines: &[DiffLine], max_rows: usize) -> Vec<PreviewRow<'_>> {
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

fn render_diff_line(line: &DiffLine, width: u16) -> Vec<Line> {
    let (sign, sign_style, body_style) = line_styles(line.kind);
    let row_style = row_style(line.kind);
    let gutter_style = row_style.patch(muted_style());
    let body_width = usize::from(width)
        .saturating_sub(INLINE_DIFF_BODY_CHROME_WIDTH)
        .max(1);
    let chunks = wrap_text(&line.content, body_width);
    let chunks = if chunks.is_empty() {
        vec![String::new()]
    } else {
        chunks
    };
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let mut spans = vec![Span::styled("  ", muted_style())];
            if index == 0 {
                spans.extend([
                    Span::styled("│ ", muted_style()),
                    Span::styled("  ", gutter_style),
                    Span::styled(
                        sign,
                        row_style.patch(sign_style.add_modifier(Modifier::BOLD)),
                    ),
                    Span::styled(format!("{:>4}", line_number(line)), gutter_style),
                    Span::styled(" │ ", gutter_style),
                    Span::styled(chunk, row_style.patch(body_style)),
                ]);
            } else {
                spans.extend([
                    Span::styled("│ ", muted_style()),
                    Span::styled("  ", gutter_style),
                    Span::styled(" ", gutter_style),
                    Span::styled("    ", gutter_style),
                    Span::styled(" │ ", gutter_style),
                    Span::styled(chunk, row_style.patch(body_style)),
                ]);
            }
            Line::from_spans(spans)
        })
        .collect()
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

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for grapheme in text.graphemes(true) {
        if current_width >= width && !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width = current_width.saturating_add(1);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
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
fn common_prefix_len(old_lines: &[&str], new_lines: &[&str]) -> usize {
    old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(old, new)| old == new)
        .count()
}
fn common_suffix_len(old_lines: &[&str], new_lines: &[&str], prefix: usize) -> usize {
    old_lines
        .iter()
        .skip(prefix)
        .rev()
        .zip(new_lines.iter().skip(prefix).rev())
        .take_while(|(old, new)| old == new)
        .count()
}
fn hunk_header(context_start: usize, old_end: usize, new_end: usize) -> String {
    format!(
        "@@ -{},{} +{},{} @@",
        context_start.saturating_add(1),
        old_end.saturating_sub(context_start),
        context_start.saturating_add(1),
        new_end.saturating_sub(context_start)
    )
}
fn changed_counts(lines: &[DiffLine]) -> (u32, u32) {
    (
        usize_to_u32(
            lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Added)
                .count(),
        ),
        usize_to_u32(
            lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Removed)
                .count(),
        ),
    )
}
fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
fn line_number(line: &DiffLine) -> String {
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

const fn row_style(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Added => Style::new().bg(Color::Indexed(22)),
        DiffLineKind::Removed => Style::new().bg(Color::Indexed(52)),
        DiffLineKind::Context | DiffLineKind::HunkHeader => Style::new(),
    }
}

const fn line_styles(kind: DiffLineKind) -> (&'static str, Style, Style) {
    match kind {
        DiffLineKind::Added => (
            "+",
            Style::new().fg(Color::BrightGreen),
            Style::new().fg(Color::BrightGreen),
        ),
        DiffLineKind::Removed => (
            "-",
            Style::new().fg(Color::BrightRed),
            Style::new().fg(Color::BrightRed),
        ),
        DiffLineKind::HunkHeader => (
            "·",
            Style::new().fg(Color::BrightCyan),
            Style::new().fg(Color::BrightCyan),
        ),
        DiffLineKind::Context => (" ", muted_style(), Style::new()),
    }
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
        let rows = file_change_rows("src/lib.rs", "", &new_text, Some("Applying"), 80);
        let rendered = format!("{rows:?}");
        assert!(rendered.contains("src/lib.rs  +40 -0"));
        assert!(rendered.contains("diff rows hidden"));
    }
}
