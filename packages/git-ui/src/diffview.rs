//! Diff presentation: language-aware highlighting projected onto diff rows.
//!
//! Each side of a file (old and new) is highlighted as a whole so multi-line constructs such
//! as block comments and strings keep their colors inside every hunk. Tokens are then mapped
//! onto diff lines by line number: context and added lines take new-side tokens, removed lines
//! take old-side tokens. When a side is unavailable or too large, each hunk side is highlighted
//! as a contiguous sequence instead.

use bcode_git::models::{DiffFile, DiffFileStatus, DiffHunk, DiffLineId, DiffLineKind};
use bcode_syntax_render::{SyntaxHighlighter, SyntaxSpan};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};

use crate::theme::GitTheme;

/// Classification of a presented row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Informational text (commit headers, messages).
    Info,
    /// A file header in a multi-file diff.
    FileHeader,
    /// A hunk header.
    HunkHeader,
    /// Unchanged context.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
}

impl RowKind {
    /// Whether the row is a stageable change.
    #[must_use]
    pub const fn is_change(self) -> bool {
        matches!(self, Self::Added | Self::Removed)
    }
}

/// One presented row with its source identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    /// Styled content.
    pub line: Line,
    /// Row classification.
    pub kind: RowKind,
    /// Index of the file within the presented diff.
    pub file: Option<usize>,
    /// Hunk index within the file.
    pub hunk: Option<usize>,
    /// Line identity within the file, for change and context rows.
    pub id: Option<DiffLineId>,
    /// New-side line number, for opening an editor at this row.
    pub new_line: Option<u32>,
    /// Leading spans that form the gutter (line numbers and sign); wrapped continuation rows
    /// repeat them as blank padding so content stays aligned.
    pub gutter_spans: usize,
    /// Whether the row may wrap. Art is kept whole and clipped at the edge instead.
    pub wrap: bool,
    /// Narrower rendering of a fixed row, used when `line` does not fit the view.
    pub compact: Option<Line>,
}

impl DiffRow {
    /// Row that never wraps (clipped at the edge), for fixed-width art.
    #[must_use]
    pub const fn fixed(line: Line) -> Self {
        let mut row = Self::info(line);
        row.wrap = false;
        row
    }

    /// Plain informational row.
    #[must_use]
    pub const fn info(line: Line) -> Self {
        Self {
            line,
            kind: RowKind::Info,
            file: None,
            hunk: None,
            id: None,
            new_line: None,
            gutter_spans: 0,
            wrap: true,
            compact: None,
        }
    }
}

/// Full text of each side of a file, when available.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideTexts {
    /// Old-side content.
    pub old: Option<String>,
    /// New-side content.
    pub new: Option<String>,
}

/// Options for presenting a diff.
#[derive(Debug, Clone, Copy)]
pub struct DiffOptions {
    /// Language-aware highlighting.
    pub highlight: bool,
    /// Emit a header row before each file.
    pub file_headers: bool,
}

/// Present one file's diff as rows.
#[must_use]
pub fn present_file(
    file_index: usize,
    file: &DiffFile,
    sides: &SideTexts,
    theme: &GitTheme,
    highlighter: &SyntaxHighlighter,
    options: DiffOptions,
) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    if options.file_headers {
        rows.push(file_header(file_index, file, theme));
    }
    if file.is_binary {
        rows.push(DiffRow::info(Line::from_spans(vec![Span::styled(
            "  Binary file differs",
            theme.muted,
        )])));
        return rows;
    }
    if file.hunks.is_empty() {
        let message = match file.status {
            DiffFileStatus::Renamed => "  Renamed without content changes",
            _ => "  No content changes (mode or metadata only)",
        };
        rows.push(DiffRow::info(Line::from_spans(vec![Span::styled(
            message,
            theme.muted,
        )])));
        return rows;
    }

    let path = file.path();
    let can_highlight = options.highlight && highlighter.can_highlight(path);
    let old_tokens = can_highlight
        .then(|| {
            sides
                .old
                .as_deref()
                .map(|text| highlight_text(highlighter, path, text))
        })
        .flatten();
    let new_tokens = can_highlight
        .then(|| {
            sides
                .new
                .as_deref()
                .map(|text| highlight_text(highlighter, path, text))
        })
        .flatten();
    let gutter_width = gutter_width(file);

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        rows.push(hunk_header(file_index, hunk_index, hunk, theme));
        let fallback = if can_highlight && (old_tokens.is_none() || new_tokens.is_none()) {
            Some(highlight_hunk(highlighter, path, hunk))
        } else {
            None
        };
        let emphasis = intraline_ranges(hunk);
        for (line_index, line) in hunk.lines.iter().enumerate() {
            let content = line.content.strip_suffix('\r').unwrap_or(&line.content);
            let tokens = if can_highlight {
                let (side_tokens, number) = match line.kind {
                    DiffLineKind::Removed => (old_tokens.as_ref(), line.old_line),
                    _ => (new_tokens.as_ref(), line.new_line),
                };
                side_tokens
                    .and_then(|tokens| {
                        let index = usize::try_from(number?).ok()?.checked_sub(1)?;
                        tokens.get(index)
                    })
                    .filter(|tokens| spans_match(tokens, content))
                    .cloned()
                    .or_else(|| {
                        fallback
                            .as_ref()
                            .and_then(|fallback| fallback.get(line_index).cloned())
                            .filter(|tokens| spans_match(tokens, content))
                    })
            } else {
                None
            };
            let row = line_spans(
                line,
                content,
                tokens,
                emphasis.get(&line_index).copied(),
                gutter_width,
                theme,
            );
            let kind = match line.kind {
                DiffLineKind::Added => RowKind::Added,
                DiffLineKind::Removed => RowKind::Removed,
                DiffLineKind::Context => RowKind::Context,
            };
            rows.push(DiffRow {
                kind,
                file: Some(file_index),
                hunk: Some(hunk_index),
                id: Some(DiffLineId {
                    hunk: hunk_index,
                    line: line_index,
                }),
                new_line: line.new_line.or(line.old_line),
                gutter_spans: 2,
                ..DiffRow::info(Line::from_spans(row))
            });
        }
    }
    rows
}

/// Styled spans for one diff line: gutter, sign, highlighted content and emphasis.
fn line_spans(
    line: &bcode_git::models::DiffLine,
    content: &str,
    tokens: Option<Vec<SyntaxSpan>>,
    emphasis: Option<(usize, usize)>,
    gutter_width: usize,
    theme: &GitTheme,
) -> Vec<Span> {
    let mut spans: Vec<Span> = tokens.map_or_else(
        || vec![Span::styled(content.to_owned(), theme.text)],
        |tokens| {
            tokens
                .into_iter()
                .map(|token| {
                    let style = syntax_style(theme, &token);
                    Span::styled(token.content, style)
                })
                .collect()
        },
    );
    let (row_style, sign, sign_style) = match line.kind {
        DiffLineKind::Added => (theme.added_row, "+", theme.added),
        DiffLineKind::Removed => (theme.removed_row, "-", theme.removed),
        DiffLineKind::Context => (Style::new(), " ", theme.muted),
    };
    if let Some((start, end)) = emphasis {
        let emphasis_style = if line.kind == DiffLineKind::Added {
            theme.added_emphasis
        } else {
            theme.removed_emphasis
        };
        spans = style_byte_range(spans, start, end, emphasis_style);
    }
    let mut row = vec![
        Span::styled(
            format!(
                "{:>width$} {:>width$} ",
                line.old_line.map_or_else(String::new, |n| n.to_string()),
                line.new_line.map_or_else(String::new, |n| n.to_string()),
                width = gutter_width
            ),
            theme.gutter,
        ),
        Span::styled(
            sign,
            sign_style
                .add_modifier(Modifier::BOLD)
                .patch(row_bg(row_style)),
        ),
    ];
    row.extend(spans.into_iter().map(|span| {
        let style = row_bg(row_style).patch(span.style);
        Span::styled(expand_tabs(&span.content), style)
    }));
    if line.no_newline {
        row.push(Span::styled(" ⏎̸", theme.muted));
    }
    row
}

fn row_bg(style: Style) -> Style {
    style.bg.map_or_else(Style::new, |bg| Style::new().bg(bg))
}

fn file_header(file_index: usize, file: &DiffFile, theme: &GitTheme) -> DiffRow {
    let (badge, style) = match file.status {
        DiffFileStatus::Added => ("A", theme.added),
        DiffFileStatus::Deleted => ("D", theme.removed),
        DiffFileStatus::Renamed => ("R", theme.info),
        DiffFileStatus::Copied => ("C", theme.info),
        DiffFileStatus::Modified => ("M", theme.warning),
    };
    let mut spans = vec![
        Span::styled("▍", theme.accent),
        Span::styled(format!("{badge} "), style.add_modifier(Modifier::BOLD)),
    ];
    if let (DiffFileStatus::Renamed | DiffFileStatus::Copied, Some(old)) =
        (file.status, file.old_path.as_deref())
    {
        spans.push(Span::styled(format!("{old} → "), theme.muted));
    }
    spans.push(Span::styled(
        file.path().to_owned(),
        theme.text.add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(format!("  +{}", file.additions), theme.added));
    spans.push(Span::styled(format!(" -{}", file.deletions), theme.removed));
    DiffRow {
        line: Line::from_spans(spans),
        kind: RowKind::FileHeader,
        file: Some(file_index),
        hunk: None,
        id: None,
        new_line: None,
        gutter_spans: 0,
        wrap: true,
        compact: None,
    }
}

fn hunk_header(file_index: usize, hunk_index: usize, hunk: &DiffHunk, theme: &GitTheme) -> DiffRow {
    let mut spans = vec![Span::styled(
        format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ),
        theme.hunk,
    )];
    if let Some(heading) = &hunk.heading {
        spans.push(Span::styled(format!(" {heading}"), theme.muted));
    }
    DiffRow {
        line: Line::from_spans(spans),
        kind: RowKind::HunkHeader,
        file: Some(file_index),
        hunk: Some(hunk_index),
        id: None,
        new_line: Some(hunk.new_start),
        gutter_spans: 0,
        wrap: true,
        compact: None,
    }
}

fn gutter_width(file: &DiffFile) -> usize {
    let max = file
        .hunks
        .iter()
        .map(|hunk| (hunk.old_start + hunk.old_count).max(hunk.new_start + hunk.new_count))
        .max()
        .unwrap_or(1);
    // Line numbers are ASCII digits, so their digit count is their cell width.
    max.max(1).ilog10() as usize + 1
}

fn highlight_text(highlighter: &SyntaxHighlighter, path: &str, text: &str) -> Vec<Vec<SyntaxSpan>> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    highlighter.highlight_lines_tokens(path, &lines)
}

/// Highlight each side of a hunk as one sequence; returns tokens indexed like `hunk.lines`.
fn highlight_hunk(
    highlighter: &SyntaxHighlighter,
    path: &str,
    hunk: &DiffHunk,
) -> Vec<Vec<SyntaxSpan>> {
    let content = |line: &bcode_git::models::DiffLine| {
        line.content
            .strip_suffix('\r')
            .unwrap_or(&line.content)
            .to_owned()
    };
    let old: Vec<String> = hunk
        .lines
        .iter()
        .filter(|line| line.kind != DiffLineKind::Added)
        .map(content)
        .collect();
    let new: Vec<String> = hunk
        .lines
        .iter()
        .filter(|line| line.kind != DiffLineKind::Removed)
        .map(content)
        .collect();
    let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
    let mut old_tokens = highlighter
        .highlight_lines_tokens(path, &old_refs)
        .into_iter();
    let mut new_tokens = highlighter
        .highlight_lines_tokens(path, &new_refs)
        .into_iter();
    hunk.lines
        .iter()
        .map(|line| match line.kind {
            DiffLineKind::Removed => old_tokens.next().unwrap_or_default(),
            DiffLineKind::Added => new_tokens.next().unwrap_or_default(),
            DiffLineKind::Context => {
                old_tokens.next();
                new_tokens.next().unwrap_or_default()
            }
        })
        .collect()
}

fn spans_match(tokens: &[SyntaxSpan], content: &str) -> bool {
    let mut rest = content;
    for token in tokens {
        match rest.strip_prefix(token.content.as_str()) {
            Some(remaining) => rest = remaining,
            None => return false,
        }
    }
    rest.is_empty()
}

const fn syntax_style(theme: &GitTheme, token: &SyntaxSpan) -> Style {
    let mut style = theme.syntax(token.style.role);
    if token.style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if token.style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if token.style.underline {
        style = style.add_modifier(Modifier::UNDERLINE);
    }
    style
}

/// Present tabs as four spaces; terminal tab stops are not reproduced inside rows.
fn expand_tabs(text: &str) -> String {
    if text.contains('\t') {
        text.replace('\t', "    ")
    } else {
        text.to_owned()
    }
}

/// Byte ranges of changed text for paired removed/added lines, keyed by line index.
fn intraline_ranges(hunk: &DiffHunk) -> std::collections::BTreeMap<usize, (usize, usize)> {
    let mut ranges = std::collections::BTreeMap::new();
    let lines = &hunk.lines;
    let mut index = 0;
    while index < lines.len() {
        if lines[index].kind != DiffLineKind::Removed {
            index += 1;
            continue;
        }
        let removed_start = index;
        while index < lines.len() && lines[index].kind == DiffLineKind::Removed {
            index += 1;
        }
        let added_start = index;
        while index < lines.len() && lines[index].kind == DiffLineKind::Added {
            index += 1;
        }
        let removed = added_start - removed_start;
        let added = index - added_start;
        if removed != added {
            continue;
        }
        for offset in 0..removed {
            let old = lines[removed_start + offset].content.trim_end_matches('\r');
            let new = lines[added_start + offset].content.trim_end_matches('\r');
            if let Some((old_range, new_range)) = changed_ranges(old, new) {
                ranges.insert(removed_start + offset, old_range);
                ranges.insert(added_start + offset, new_range);
            }
        }
    }
    ranges
}

/// Common-prefix/suffix change detection on character boundaries. Returns `None` when the lines
/// are unrelated enough that emphasis would be noise.
fn changed_ranges(old: &str, new: &str) -> Option<((usize, usize), (usize, usize))> {
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let suffix: usize = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    let changed = (old_end - prefix).max(new_end - prefix);
    let longest = old.len().max(new.len()).max(1);
    if changed == 0 || changed * 10 > longest * 7 || prefix + suffix == 0 {
        return None;
    }
    Some(((prefix, old_end), (prefix, new_end)))
}

/// Patch `style` onto the bytes `start..end` of the concatenated span text, splitting spans on
/// character boundaries as needed.
fn style_byte_range(spans: Vec<Span>, start: usize, end: usize, style: Style) -> Vec<Span> {
    let mut output = Vec::with_capacity(spans.len() + 2);
    let mut offset = 0;
    for span in spans {
        let len = span.content.len();
        let (span_start, span_end) = (offset, offset + len);
        offset = span_end;
        if span_end <= start || span_start >= end {
            output.push(span);
            continue;
        }
        let local_start = start.saturating_sub(span_start);
        let local_end = (end - span_start).min(len);
        let content = span.content.as_str();
        if !content.is_char_boundary(local_start) || !content.is_char_boundary(local_end) {
            output.push(span);
            continue;
        }
        if local_start > 0 {
            output.push(Span::styled(content[..local_start].to_owned(), span.style));
        }
        output.push(Span::styled(
            content[local_start..local_end].to_owned(),
            span.style.patch(style),
        ));
        if local_end < len {
            output.push(Span::styled(content[local_end..].to_owned(), span.style));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_git::diff::parse_unified_diff;

    fn theme() -> GitTheme {
        GitTheme::fallback()
    }

    fn file(diff: &str) -> DiffFile {
        parse_unified_diff(diff).unwrap().remove(0)
    }

    fn text(line: &Line) -> String {
        line.plain_text()
    }

    const OPTIONS: DiffOptions = DiffOptions {
        highlight: true,
        file_headers: false,
    };

    #[test]
    fn rows_carry_identities_and_preserve_content() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n";
        let rows = present_file(
            0,
            &file(diff),
            &SideTexts::default(),
            &theme(),
            &SyntaxHighlighter::new(),
            OPTIONS,
        );
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, RowKind::HunkHeader);
        assert_eq!(rows[2].kind, RowKind::Removed);
        assert_eq!(rows[3].id, Some(DiffLineId { hunk: 0, line: 2 }));
        assert!(text(&rows[3].line).ends_with("+    let x = 2;"));
    }

    #[test]
    fn whole_file_highlighting_keeps_block_comment_context_inside_hunks() {
        let new = "/* start\nstill comment\nend */\nfn main() {}\n";
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,1 +2,1 @@\n-old comment\n+still comment\n";
        let theme = theme();
        let rows = present_file(
            0,
            &file(diff),
            &SideTexts {
                old: Some("/* start\nold comment\nend */\nfn main() {}\n".into()),
                new: Some(new.into()),
            },
            &theme,
            &SyntaxHighlighter::new(),
            OPTIONS,
        );
        let added = rows.iter().find(|row| row.kind == RowKind::Added).unwrap();
        let content_style = added.line.spans.last().unwrap().style;
        assert_eq!(
            content_style.fg,
            theme.syntax(bcode_syntax_render::SyntaxRole::Comment).fg
        );
    }

    #[test]
    fn mismatched_side_text_falls_back_without_losing_content() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let rows = present_file(
            0,
            &file(diff),
            &SideTexts {
                old: Some("zzz\n".into()),
                new: Some("yyy\n".into()),
            },
            &theme(),
            &SyntaxHighlighter::new(),
            OPTIONS,
        );
        assert!(text(&rows[1].line).ends_with("-a"));
        assert!(text(&rows[2].line).ends_with("+b"));
    }

    #[test]
    fn intraline_ranges_respect_unicode_boundaries() {
        assert_eq!(
            changed_ranges("let 名前 = 1;", "let 名前 = 22;"),
            Some(((13, 14), (13, 15)))
        );
        assert_eq!(changed_ranges("abc", "xyz"), None);
        let spans = style_byte_range(
            vec![Span::raw("ab"), Span::raw("cd")],
            1,
            3,
            Style::new().add_modifier(Modifier::BOLD),
        );
        let contents: Vec<&str> = spans.iter().map(|span| span.content.as_str()).collect();
        assert_eq!(contents, ["a", "b", "c", "d"]);
    }
}
