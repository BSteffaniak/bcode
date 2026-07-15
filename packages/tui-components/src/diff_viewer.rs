//! Generic diff viewer component for Bcode TUI presentations.

use crate::source_viewer::pad_card_spans;
#[cfg(feature = "syntax")]
use bcode_syntax_render::SyntaxHighlighter;
#[cfg(feature = "syntax")]
use bcode_syntax_render::SyntaxStyle;
use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Syntax-highlighted span content for diff lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSyntaxSpan {
    /// Span text.
    pub content: String,
    /// Optional syntax style.
    #[cfg(feature = "syntax")]
    pub style: SyntaxStyle,
}

#[cfg(feature = "syntax")]
impl From<bcode_syntax_render::SyntaxSpan> for DiffSyntaxSpan {
    fn from(span: bcode_syntax_render::SyntaxSpan) -> Self {
        Self {
            content: span.content,
            style: span.style,
        }
    }
}

const DIFF_CONTEXT_LINES: usize = 3;
const MAX_LCS_CELLS: usize = 40_000;
const MAX_INTRALINE_LINE_GRAPHEMES: usize = 2_000;

/// Kind of a rendered diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    FileHeader,
    HunkHeader,
    Context,
    Added,
    Removed,
}

/// Byte range that changed within a diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangedRange {
    pub start: usize,
    pub end: usize,
}

impl ChangedRange {
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// One logical diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub content: String,
    pub changed_ranges: Vec<ChangedRange>,
    pub syntax_spans: Vec<DiffSyntaxSpan>,
}

impl DiffLine {
    #[must_use]
    pub fn new(
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
            changed_ranges: Vec::new(),
            syntax_spans: Vec::new(),
        }
    }
}

/// Complete diff document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffDocument {
    pub label: String,
    pub lines: Vec<DiffLine>,
    pub added: u32,
    pub removed: u32,
}

/// Build a diff document from old and new text.
#[must_use]
pub fn diff_from_text(label: &str, old_text: &str, new_text: &str) -> DiffDocument {
    diff_from_text_at_lines(label, old_text, new_text, 1, 1)
}

/// Build a diff document whose input fragments begin at the given file lines.
#[must_use]
pub fn diff_from_text_at_lines(
    label: &str,
    old_text: &str,
    new_text: &str,
    old_start_line: u32,
    new_start_line: u32,
) -> DiffDocument {
    let old_offset = old_start_line.saturating_sub(1);
    let new_offset = new_start_line.saturating_sub(1);
    let mut lines = diff_lines_from_text(label, old_text, new_text);
    for line in &mut lines {
        line.old_line = line
            .old_line
            .map(|number| number.saturating_add(old_offset));
        line.new_line = line
            .new_line
            .map(|number| number.saturating_add(new_offset));
        if line.kind == DiffLineKind::HunkHeader {
            line.content = offset_hunk_header(&line.content, old_offset, new_offset);
        }
    }
    apply_intraline_changed_ranges(&mut lines);
    apply_syntax_highlighting(label, &mut lines);
    let (added, removed) = count_changed_diff_lines(&lines);
    DiffDocument {
        label: label.to_owned(),
        lines,
        added,
        removed,
    }
}

fn diff_lines_from_text(label: &str, old_text: &str, new_text: &str) -> Vec<DiffLine> {
    let old_lines = old_text.lines().collect::<Vec<_>>();
    let new_lines = new_text.lines().collect::<Vec<_>>();
    let mut lines = file_headers(label);
    if old_lines == new_lines {
        push_unchanged_preview(&mut lines, &old_lines);
        return lines;
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

    lines.push(DiffLine::new(
        DiffLineKind::HunkHeader,
        None,
        None,
        hunk_header(context_start, old_context_end, new_context_end),
    ));
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

fn file_headers(label: &str) -> Vec<DiffLine> {
    vec![
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("--- {label}")),
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("+++ {label}")),
    ]
}

fn push_unchanged_preview(lines: &mut Vec<DiffLine>, old_lines: &[&str]) {
    let preview_len = old_lines.len().min(DIFF_CONTEXT_LINES.saturating_mul(2));
    for (index, line) in old_lines.iter().take(preview_len).enumerate() {
        let number = usize_to_u32(index.saturating_add(1));
        lines.push(DiffLine::new(
            DiffLineKind::Context,
            Some(number),
            Some(number),
            *line,
        ));
    }
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
            lines.push(DiffLine::new(
                DiffLineKind::Context,
                Some(usize_to_u32(
                    base.saturating_add(old_index).saturating_add(1),
                )),
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

fn apply_intraline_changed_ranges(lines: &mut [DiffLine]) {
    let mut index = 0usize;
    while index < lines.len() {
        if lines[index].kind != DiffLineKind::Removed {
            index = index.saturating_add(1);
            continue;
        }
        let removed_start = index;
        while index < lines.len() && lines[index].kind == DiffLineKind::Removed {
            index = index.saturating_add(1);
        }
        let added_start = index;
        while index < lines.len() && lines[index].kind == DiffLineKind::Added {
            index = index.saturating_add(1);
        }
        if added_start == index {
            continue;
        }
        apply_changed_ranges_to_run(lines, removed_start, added_start, index);
    }
}

fn apply_changed_ranges_to_run(
    lines: &mut [DiffLine],
    removed_start: usize,
    added_start: usize,
    added_end: usize,
) {
    let pair_count = added_start
        .saturating_sub(removed_start)
        .min(added_end.saturating_sub(added_start));
    for offset in 0..pair_count {
        let removed_index = removed_start.saturating_add(offset);
        let added_index = added_start.saturating_add(offset);
        let removed = lines[removed_index].content.clone();
        let added = lines[added_index].content.clone();
        if let Some((removed_ranges, added_ranges)) = changed_ranges_for_pair(&removed, &added) {
            lines[removed_index].changed_ranges = removed_ranges;
            lines[added_index].changed_ranges = added_ranges;
        }
    }
}

fn changed_ranges_for_pair(old: &str, new: &str) -> Option<(Vec<ChangedRange>, Vec<ChangedRange>)> {
    let old_graphemes = grapheme_bounds(old);
    let new_graphemes = grapheme_bounds(new);
    if old_graphemes.len() > MAX_INTRALINE_LINE_GRAPHEMES
        || new_graphemes.len() > MAX_INTRALINE_LINE_GRAPHEMES
    {
        return None;
    }
    let prefix = old_graphemes
        .iter()
        .zip(new_graphemes.iter())
        .take_while(|(old, new)| old.2 == new.2)
        .count();
    let suffix = old_graphemes
        .iter()
        .skip(prefix)
        .rev()
        .zip(new_graphemes.iter().skip(prefix).rev())
        .take_while(|(old, new)| old.2 == new.2)
        .count();
    let old_end = old_graphemes.len().saturating_sub(suffix);
    let new_end = new_graphemes.len().saturating_sub(suffix);
    Some((
        range_from_graphemes(&old_graphemes, prefix, old_end),
        range_from_graphemes(&new_graphemes, prefix, new_end),
    ))
}

fn grapheme_bounds(text: &str) -> Vec<(usize, usize, &str)> {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| (start, start.saturating_add(grapheme.len()), grapheme))
        .collect()
}

fn range_from_graphemes(
    graphemes: &[(usize, usize, &str)],
    start: usize,
    end: usize,
) -> Vec<ChangedRange> {
    if start >= end || start >= graphemes.len() {
        return Vec::new();
    }
    let end_index = end.saturating_sub(1).min(graphemes.len().saturating_sub(1));
    vec![ChangedRange::new(
        graphemes[start].0,
        graphemes[end_index].1,
    )]
}

#[cfg(feature = "syntax")]
fn apply_syntax_highlighting(label: &str, lines: &mut [DiffLine]) {
    let highlighter = SyntaxHighlighter::new();
    if !highlighter.can_highlight(label) {
        return;
    }
    for line in lines.iter_mut().filter(|line| {
        matches!(
            line.kind,
            DiffLineKind::Context | DiffLineKind::Added | DiffLineKind::Removed
        )
    }) {
        line.syntax_spans = highlighter
            .highlight_line_tokens(label, &line.content)
            .into_iter()
            .map(Into::into)
            .collect();
    }
}

#[cfg(not(feature = "syntax"))]
const fn apply_syntax_highlighting(_label: &str, _lines: &mut [DiffLine]) {}

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

fn offset_hunk_header(header: &str, old_offset: u32, new_offset: u32) -> String {
    let Some((old_range, new_range)) = header
        .strip_prefix("@@ -")
        .and_then(|value| value.strip_suffix(" @@"))
        .and_then(|value| value.split_once(" +"))
    else {
        return header.to_owned();
    };
    let offset_range = |range: &str, offset: u32| {
        let (start, count) = range.split_once(',').unwrap_or((range, "1"));
        let start = start.parse::<u32>().unwrap_or(1).saturating_add(offset);
        format!("{start},{count}")
    };
    format!(
        "@@ -{} +{} @@",
        offset_range(old_range, old_offset),
        offset_range(new_range, new_offset)
    )
}

fn count_changed_diff_lines(lines: &[DiffLine]) -> (u32, u32) {
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

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn diff_includes_headers_context_and_counts() {
        let diff = diff_from_text("src/lib.rs", "a\nb\nc\n", "a\nbb\nc\n");
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.lines[0].content, "--- src/lib.rs");
        assert_eq!(diff.lines[1].content, "+++ src/lib.rs");
        assert!(
            diff.lines
                .iter()
                .any(|line| line.kind == DiffLineKind::HunkHeader)
        );
    }

    #[test]
    fn long_changed_lines_receive_intraline_highlighting() {
        let prefix = "a".repeat(120);
        let old = format!("{prefix}old suffix");
        let new = format!("{prefix}new suffix");
        let diff = diff_from_text("src/lib.rs", &old, &new);
        let removed = diff
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Removed)
            .expect("removed line");
        let added = diff
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Added)
            .expect("added line");

        assert!(!removed.changed_ranges.is_empty());
        assert!(!added.changed_ranges.is_empty());
    }

    #[test]
    fn changed_ranges_are_unicode_boundary_safe() {
        let diff = diff_from_text("src/lib.rs", "let face = \"😀\";\n", "let face = \"😃\";\n");
        let added = diff
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Added)
            .expect("added line");
        assert!(added.changed_ranges.iter().all(|range| {
            added.content.is_char_boundary(range.start) && added.content.is_char_boundary(range.end)
        }));
    }

    #[cfg(feature = "syntax")]
    #[test]
    fn syntax_highlighting_is_plugin_owned() {
        let diff = diff_from_text("src/main.rs", "", "fn main() {}\n");
        assert!(diff.lines.iter().any(|line| !line.syntax_spans.is_empty()));
    }
}

const MAX_INLINE_DIFF_ROWS: usize = 24;
const INLINE_DIFF_CARD_MIN_WIDTH: usize = 24;
const INLINE_DIFF_CARD_CHROME_WIDTH: usize = 14;
const INLINE_DIFF_BODY_CHROME_WIDTH: usize = 14;

#[derive(Debug, Clone, Copy)]
enum PreviewRow<'a> {
    Line(&'a DiffLine),
    Hidden {
        count: usize,
        old_count: usize,
        new_count: usize,
    },
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

/// Diff viewer layout policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffViewerLayout {
    /// Render one full-width stream.
    Unified,
    /// Render old and new content in adjacent columns.
    SideBySide,
    /// Select side-by-side at or above the supplied component width.
    Auto { breakpoint: u16 },
}

impl Default for DiffViewerLayout {
    fn default() -> Self {
        Self::Auto { breakpoint: 120 }
    }
}

impl DiffViewerLayout {
    /// Resolve an automatic policy for the actual component width.
    #[must_use]
    pub const fn resolve(self, width: u16) -> Self {
        match self {
            Self::Auto { breakpoint } if width >= breakpoint => Self::SideBySide,
            Self::Auto { .. } => Self::Unified,
            fixed => fixed,
        }
    }
}

/// Input used to render diff viewer rows.
#[derive(Debug, Clone, Copy)]
pub struct DiffViewerInput<'a> {
    /// Label displayed for the changed content.
    pub label: &'a str,
    /// Text before the change.
    pub old_text: &'a str,
    /// Text after the change.
    pub new_text: &'a str,
    /// First file line represented by `old_text`.
    pub old_start_line: u32,
    /// First file line represented by `new_text`.
    pub new_start_line: u32,
    /// Title rendered above the diff.
    pub title: &'a str,
    /// Optional subtitle rendered above the diff.
    pub subtitle: Option<&'a str>,
    /// Optional original argument size for truncation messaging.
    pub argument_bytes: Option<usize>,
    /// Whether input text was truncated before diffing.
    pub truncated: bool,
    /// Layout policy for this rendering.
    pub layout: DiffViewerLayout,
}

/// Render diff viewer rows.
#[must_use]
pub fn diff_viewer_rows(input: DiffViewerInput<'_>, width: u16) -> Vec<Line> {
    let diff = diff_from_text_at_lines(
        input.label,
        input.old_text,
        input.new_text,
        input.old_start_line,
        input.new_start_line,
    );
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
            format!("{}  +{} -{}", diff.label, diff.added, diff.removed),
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
    let resolved_layout = resolved_layout(input.layout, width, diff.added, diff.removed);
    let card_width = if resolved_layout == DiffViewerLayout::SideBySide {
        side_by_side_card_width(&preview, width.saturating_sub(2))
    } else {
        card_width(&preview, width.saturating_sub(2))
    };
    rows.push(card_border('┌', '─', '┐', card_width));
    if resolved_layout == DiffViewerLayout::SideBySide {
        rows.extend(render_side_by_side_preview(&preview, card_width));
    } else {
        for row in preview {
            match row {
                PreviewRow::Line(line) => rows.extend(render_diff_line(line, card_width)),
                PreviewRow::Hidden { count, .. } => rows.push(hidden_row(count, card_width)),
            }
        }
    }
    rows.push(card_border('└', '─', '┘', card_width));
    rows
}

const fn resolved_layout(
    layout: DiffViewerLayout,
    width: u16,
    added: u32,
    removed: u32,
) -> DiffViewerLayout {
    if added == 0 || removed == 0 {
        DiffViewerLayout::Unified
    } else {
        layout.resolve(width)
    }
}

const fn is_preview_content_line(kind: DiffLineKind) -> bool {
    matches!(
        kind,
        DiffLineKind::Added | DiffLineKind::Removed | DiffLineKind::Context
    )
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
        .chain(std::iter::once({
            let hidden_lines = &lines[head..lines.len().saturating_sub(tail)];
            let old_count = hidden_lines
                .iter()
                .filter(|line| line.old_line.is_some())
                .count();
            let new_count = hidden_lines
                .iter()
                .filter(|line| line.new_line.is_some())
                .count();
            PreviewRow::Hidden {
                count: hidden,
                old_count,
                new_count,
            }
        }))
        .chain(
            lines
                .iter()
                .skip(lines.len().saturating_sub(tail))
                .map(PreviewRow::Line),
        )
        .collect()
}

const SPLIT_CELL_CHROME_WIDTH: usize = 9;
const SPLIT_CELL_MIN_WIDTH: usize = SPLIT_CELL_CHROME_WIDTH + 1;

fn side_by_side_content_widths(preview: &[PreviewRow<'_>]) -> (usize, usize) {
    preview.iter().fold((0, 0), |(old, new), row| match row {
        PreviewRow::Line(line) => {
            let width = UnicodeWidthStr::width(line.content.as_str());
            match line.kind {
                DiffLineKind::Removed => (old.max(width), new),
                DiffLineKind::Added => (old, new.max(width)),
                DiffLineKind::Context | DiffLineKind::HunkHeader | DiffLineKind::FileHeader => {
                    (old.max(width), new.max(width))
                }
            }
        }
        PreviewRow::Hidden { .. } => (old, new),
    })
}

fn side_by_side_card_width(preview: &[PreviewRow<'_>], available_width: u16) -> u16 {
    let available = usize::from(available_width.max(1));
    let (old, new) = side_by_side_content_widths(preview);
    let desired = old
        .saturating_add(new)
        .saturating_add(SPLIT_CELL_CHROME_WIDTH.saturating_mul(2))
        .saturating_add(3);
    u16::try_from(desired.clamp(INLINE_DIFF_CARD_MIN_WIDTH.min(available), available))
        .unwrap_or(u16::MAX)
}

fn side_by_side_column_widths(preview: &[PreviewRow<'_>], width: u16) -> (usize, usize) {
    let cells_width = usize::from(width).saturating_sub(3);
    let (old_content, new_content) = side_by_side_content_widths(preview);
    let old_desired = old_content.saturating_add(SPLIT_CELL_CHROME_WIDTH);
    let new_desired = new_content.saturating_add(SPLIT_CELL_CHROME_WIDTH);
    let desired_total = old_desired.saturating_add(new_desired);
    if desired_total <= cells_width {
        return (old_desired, cells_width.saturating_sub(old_desired));
    }

    let distributable = cells_width.saturating_sub(SPLIT_CELL_MIN_WIDTH.saturating_mul(2));
    let content_total = old_content.saturating_add(new_content).max(1);
    let old_extra = distributable.saturating_mul(old_content) / content_total;
    let old_width = SPLIT_CELL_MIN_WIDTH.saturating_add(old_extra);
    (old_width, cells_width.saturating_sub(old_width))
}

fn render_side_by_side_preview(preview: &[PreviewRow<'_>], width: u16) -> Vec<Line> {
    let mut rows = Vec::new();
    let (left_width, right_width) = side_by_side_column_widths(preview, width);
    let mut index = 0;
    while index < preview.len() {
        match preview[index] {
            PreviewRow::Hidden {
                count,
                old_count,
                new_count,
            } => {
                rows.push(split_hidden_row(
                    count,
                    old_count,
                    new_count,
                    width,
                    left_width,
                    right_width,
                ));
                index += 1;
            }
            PreviewRow::Line(line) if line.kind == DiffLineKind::Removed => {
                let removed_start = index;
                while matches!(preview.get(index), Some(PreviewRow::Line(line)) if line.kind == DiffLineKind::Removed)
                {
                    index += 1;
                }
                let added_start = index;
                while matches!(preview.get(index), Some(PreviewRow::Line(line)) if line.kind == DiffLineKind::Added)
                {
                    index += 1;
                }
                let removed_count = added_start.saturating_sub(removed_start);
                let added_count = index.saturating_sub(added_start);
                for offset in 0..removed_count.max(added_count) {
                    let old = (offset < removed_count)
                        .then(|| preview.get(removed_start + offset).and_then(preview_line))
                        .flatten();
                    let new = (offset < added_count)
                        .then(|| preview.get(added_start + offset).and_then(preview_line))
                        .flatten();
                    rows.extend(render_split_row(old, new, left_width, right_width));
                }
            }
            PreviewRow::Line(line) if line.kind == DiffLineKind::Added => {
                rows.extend(render_split_row(None, Some(line), left_width, right_width));
                index += 1;
            }
            PreviewRow::Line(line) => {
                rows.extend(render_split_row(
                    Some(line),
                    Some(line),
                    left_width,
                    right_width,
                ));
                index += 1;
            }
        }
    }
    rows
}

fn split_hidden_row(
    count: usize,
    old_count: usize,
    new_count: usize,
    width: u16,
    old_width: usize,
    new_width: usize,
) -> Line {
    let text = format!("⋯ {count} rows omitted ⋯");
    if old_count > 0 && new_count > 0 {
        let inner_width = usize::from(width).saturating_sub(2);
        let clipped = truncate_to_display_width(&text, inner_width);
        let text_width = UnicodeWidthStr::width(clipped.as_str());
        let left_padding = inner_width.saturating_sub(text_width) / 2;
        let right_padding = inner_width
            .saturating_sub(text_width)
            .saturating_sub(left_padding);
        return Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled("│", muted_style()),
            Span::styled(" ".repeat(left_padding), muted_style()),
            Span::styled(clipped, muted_style()),
            Span::styled(" ".repeat(right_padding), muted_style()),
            Span::styled("│", muted_style()),
        ]);
    }

    let mut spans = vec![
        Span::styled("  ", muted_style()),
        Span::styled("│", muted_style()),
    ];
    spans.extend(centered_hidden_cell(
        (old_count > 0).then_some(text.as_str()),
        old_width,
    ));
    spans.push(Span::styled("│", muted_style()));
    spans.extend(centered_hidden_cell(
        (new_count > 0).then_some(text.as_str()),
        new_width,
    ));
    spans.push(Span::styled("│", muted_style()));
    Line::from_spans(spans)
}

fn centered_hidden_cell(text: Option<&str>, width: usize) -> Vec<Span> {
    let clipped = truncate_to_display_width(text.unwrap_or_default(), width);
    let text_width = UnicodeWidthStr::width(clipped.as_str());
    let left_padding = width.saturating_sub(text_width) / 2;
    let right_padding = width
        .saturating_sub(text_width)
        .saturating_sub(left_padding);
    vec![
        Span::styled(" ".repeat(left_padding), muted_style()),
        Span::styled(clipped, muted_style()),
        Span::styled(" ".repeat(right_padding), muted_style()),
    ]
}

const fn preview_line<'a>(row: &'a PreviewRow<'a>) -> Option<&'a DiffLine> {
    match row {
        PreviewRow::Line(line) => Some(line),
        PreviewRow::Hidden { .. } => None,
    }
}

fn render_split_row(
    old: Option<&DiffLine>,
    new: Option<&DiffLine>,
    old_width: usize,
    new_width: usize,
) -> Vec<Line> {
    let old_chunks = split_cell_chunks(old, old_width);
    let new_chunks = split_cell_chunks(new, new_width);
    let height = old_chunks.len().max(new_chunks.len());
    (0..height)
        .map(|row| {
            let mut spans = vec![
                Span::styled("  ", muted_style()),
                Span::styled("│", muted_style()),
            ];
            spans.extend(split_cell_row(old, old_chunks.get(row), row, old_width));
            spans.push(Span::styled("│", muted_style()));
            spans.extend(split_cell_row(new, new_chunks.get(row), row, new_width));
            spans.push(Span::styled("│", muted_style()));
            Line::from_spans(spans)
        })
        .collect()
}

fn split_cell_chunks(line: Option<&DiffLine>, width: usize) -> Vec<Vec<Span>> {
    let Some(line) = line else {
        return vec![Vec::new()];
    };
    let body_width = width.saturating_sub(9).max(1);
    let (_, _, body_style) = line_styles(line.kind);
    let chunks = wrap_spans(
        content_spans(line, row_style(line.kind).patch(body_style)),
        &line.changed_ranges,
        emphasis_style(line.kind),
        body_width,
    );
    if chunks.is_empty() {
        vec![Vec::new()]
    } else {
        chunks
    }
}

fn split_cell_row(
    line: Option<&DiffLine>,
    chunk: Option<&Vec<Span>>,
    row: usize,
    width: usize,
) -> Vec<Span> {
    let Some(line) = line else {
        return vec![Span::styled(" ".repeat(width), Style::new())];
    };
    let (sign, sign_style, body_style) = line_styles(line.kind);
    let background = row_style(line.kind);
    let gutter_style = background.patch(muted_style());
    let number = if row == 0 {
        (if matches!(line.kind, DiffLineKind::Removed) {
            line.old_line
        } else {
            line.new_line
        })
        .or(line.old_line)
        .map_or_else(String::new, |number| number.to_string())
    } else {
        String::new()
    };
    let mut spans = vec![
        Span::styled(" ", gutter_style),
        Span::styled(
            if row == 0 { sign } else { " " },
            background.patch(sign_style.add_modifier(Modifier::BOLD)),
        ),
        Span::styled(format!("{number:>4}"), gutter_style),
        Span::styled(" │ ", gutter_style),
    ];
    if let Some(chunk) = chunk {
        spans.extend(chunk.iter().cloned());
    }
    pad_card_spans(&mut spans, width, background.patch(body_style));
    spans
}

fn render_diff_line(line: &DiffLine, width: u16) -> Vec<Line> {
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

fn content_spans(line: &DiffLine, fallback_style: Style) -> Vec<Span> {
    #[cfg(not(feature = "syntax"))]
    {
        vec![Span::styled(line.content.clone(), fallback_style)]
    }
    #[cfg(feature = "syntax")]
    {
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
            PreviewRow::Hidden { count, .. } => {
                UnicodeWidthStr::width(hidden_text(*count).as_str())
            }
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
    let padding = inner_width.saturating_sub(clipped_width);
    let left_padding = padding / 2;
    let right_padding = padding.saturating_sub(left_padding);
    let mut spans = vec![
        Span::styled("  ", muted_style()),
        Span::styled("│ ", muted_style()),
        Span::styled(" ".repeat(left_padding), muted_style()),
        Span::styled(clipped, muted_style()),
    ];
    if right_padding > 0 {
        spans.push(Span::styled(" ".repeat(right_padding), muted_style()));
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
    format!("⋯ {count} rows omitted ⋯")
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

fn line_number(line: &DiffLine) -> String {
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

const fn row_style(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Added => Style::new().bg(Color::Rgb(0, 24, 16)),
        DiffLineKind::Removed => Style::new().bg(Color::Rgb(32, 10, 10)),
        DiffLineKind::Context | DiffLineKind::HunkHeader | DiffLineKind::FileHeader => Style::new(),
    }
}

const fn emphasis_style(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Added => Style::new().bg(Color::Rgb(0, 42, 26)),
        DiffLineKind::Removed => Style::new().bg(Color::Rgb(50, 14, 14)),
        DiffLineKind::Context | DiffLineKind::HunkHeader | DiffLineKind::FileHeader => Style::new(),
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
        DiffLineKind::FileHeader => ("·", muted_style(), muted_style()),
        DiffLineKind::Context => (" ", muted_style(), Style::new()),
    }
}

#[cfg(feature = "syntax")]
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

    fn test_diff_viewer_rows_with_layout(
        label: &str,
        old_text: &str,
        new_text: &str,
        title: &str,
        truncated: bool,
        width: u16,
        layout: DiffViewerLayout,
    ) -> Vec<Line> {
        diff_viewer_rows(
            DiffViewerInput {
                label,
                old_text,
                new_text,
                old_start_line: 1,
                new_start_line: 1,
                title,
                subtitle: None,
                argument_bytes: None,
                truncated,
                layout,
            },
            width,
        )
    }

    fn test_diff_viewer_rows(
        label: &str,
        old_text: &str,
        new_text: &str,
        title: &str,
        truncated: bool,
        width: u16,
    ) -> Vec<Line> {
        test_diff_viewer_rows_with_layout(
            label,
            old_text,
            new_text,
            title,
            truncated,
            width,
            DiffViewerLayout::Unified,
        )
    }

    #[test]
    fn one_sided_diffs_fall_back_to_unified_layout() {
        for (old_text, new_text) in [("", "new line\n"), ("old line\n", "")] {
            let rows = test_diff_viewer_rows_with_layout(
                "src/lib.rs",
                old_text,
                new_text,
                "Diff",
                false,
                120,
                DiffViewerLayout::SideBySide,
            );
            let card_rows = rows
                .iter()
                .filter(|row| {
                    let text = line_text(row);
                    text.starts_with("  │")
                })
                .collect::<Vec<_>>();

            assert!(!card_rows.is_empty(), "{rows:?}");
            for row in card_rows {
                let divider_count = row
                    .spans
                    .iter()
                    .filter(|span| span.content.contains('│'))
                    .map(|span| span.content.matches('│').count())
                    .sum::<usize>();
                assert_eq!(divider_count, 3, "{row:?}");
            }
        }
    }

    #[test]
    fn one_kind_of_change_falls_back_to_unified_layout() {
        for (old_text, new_text) in [
            ("before\nafter\n", "before\ninserted\nafter\n"),
            ("before\nremoved\nafter\n", "before\nafter\n"),
        ] {
            let rows = test_diff_viewer_rows_with_layout(
                "src/lib.rs",
                old_text,
                new_text,
                "Diff",
                false,
                120,
                DiffViewerLayout::SideBySide,
            );
            let card_rows = rows
                .iter()
                .filter(|row| line_text(row).starts_with("  │"))
                .collect::<Vec<_>>();

            assert!(!card_rows.is_empty(), "{rows:?}");
            for row in card_rows {
                let divider_count = row
                    .spans
                    .iter()
                    .filter(|span| span.content.contains('│'))
                    .map(|span| span.content.matches('│').count())
                    .sum::<usize>();
                assert_eq!(divider_count, 3, "{row:?}");
            }
        }
    }

    #[test]
    fn diff_viewer_rows_show_counts_and_hidden_rows() {
        let new_text = (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = test_diff_viewer_rows("src/lib.rs", "", &new_text, "Applying", false, 80);
        let rendered = format!("{rows:?}");
        assert!(rendered.contains("src/lib.rs  +40 -0"));
        assert!(rendered.contains("rows omitted"));
    }

    #[test]
    fn diff_viewer_rows_use_pre_migration_inline_diff_palette() {
        let rows = test_diff_viewer_rows(
            "src/lib.rs",
            "let value = 1;\n",
            "let value = 2;\n",
            "Diff",
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
    fn diff_viewer_rows_render_available_new_text_without_pending_warning() {
        let rows = test_diff_viewer_rows(
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
    fn diff_viewer_rows_use_pre_migration_progress_and_line_number_fallback() {
        let rows = test_diff_viewer_rows(
            "src/lib.rs",
            "let value = 1;\n",
            "let value = 2;\n",
            "Diff",
            false,
            80,
        );
        let rendered = format!("{rows:?}");
        assert!(
            rendered.contains("live preview · /diff for full view"),
            "{rendered}"
        );

        let line = DiffLine::new(DiffLineKind::Context, None, None, "context");
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
        let text_index = row
            .spans
            .iter()
            .position(|span| span.content.contains("rows omitted"))
            .expect("omission text");
        let left_padding = UnicodeWidthStr::width(row.spans[text_index - 1].content.as_str());
        let right_padding = UnicodeWidthStr::width(row.spans[text_index + 1].content.as_str());
        assert!(left_padding.abs_diff(right_padding) <= 1);
    }

    #[test]
    fn side_by_side_rows_align_with_card_borders_and_wrap() {
        let removed = DiffLine::new(
            DiffLineKind::Removed,
            Some(1),
            None,
            "a long removed line that must wrap across rows",
        );
        let added = DiffLine::new(
            DiffLineKind::Added,
            None,
            Some(1),
            "a long added line that must wrap across rows",
        );
        let card_width = 40;
        let rows = render_side_by_side_preview(
            &[PreviewRow::Line(&removed), PreviewRow::Line(&added)],
            card_width,
        );

        assert!(rows.len() > 1, "{rows:?}");
        for row in rows {
            assert_eq!(line_width(&row), usize::from(card_width) + 2, "{row:?}");
            assert_eq!(row.spans[0].content, "  ");
        }
    }

    #[test]
    fn side_by_side_omission_marker_uses_the_owning_column() {
        let row = split_hidden_row(19, 0, 19, 80, 38, 39);
        let center = row
            .spans
            .iter()
            .enumerate()
            .skip(2)
            .find(|(_, span)| span.content == "│")
            .map(|(index, _)| index)
            .expect("center divider");
        let left = row.spans[2..center]
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        let right = row.spans[center + 1..]
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();

        assert!(left.trim().is_empty(), "{row:?}");
        assert!(right.contains("19 rows omitted"), "{row:?}");
        assert_eq!(line_width(&row), 82);
    }

    #[test]
    fn side_by_side_does_not_put_extra_added_lines_on_old_side() {
        let removed = DiffLine::new(DiffLineKind::Removed, Some(1), None, "old");
        let added_one = DiffLine::new(DiffLineKind::Added, None, Some(1), "new one");
        let added_two = DiffLine::new(DiffLineKind::Added, None, Some(2), "new two");
        let rows = render_side_by_side_preview(
            &[
                PreviewRow::Line(&removed),
                PreviewRow::Line(&added_one),
                PreviewRow::Line(&added_two),
            ],
            80,
        );
        let second = &rows[1];
        let divider = second
            .spans
            .iter()
            .position(|span| span.content == "│")
            .expect("outer border");
        let center = second
            .spans
            .iter()
            .enumerate()
            .skip(divider + 1)
            .find(|(_, span)| span.content == "│")
            .map(|(index, _)| index)
            .expect("center divider");
        let left = second.spans[divider + 1..center]
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        let right = second.spans[center + 1..]
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();

        assert!(!left.contains("new two"), "{second:?}");
        assert!(right.contains("new two"), "{second:?}");
    }

    #[test]
    fn side_by_side_width_expands_and_favors_longer_content() {
        let removed = DiffLine::new(DiffLineKind::Removed, Some(1), None, "old");
        let added = DiffLine::new(
            DiffLineKind::Added,
            None,
            Some(1),
            "a substantially longer replacement line that needs more room",
        );
        let preview = [PreviewRow::Line(&removed), PreviewRow::Line(&added)];

        let narrow_card = side_by_side_card_width(&preview, 60);
        let wide_card = side_by_side_card_width(&preview, 120);
        let (old_width, new_width) = side_by_side_column_widths(&preview, wide_card);

        assert_eq!(narrow_card, 60);
        assert!(wide_card > narrow_card);
        assert!(new_width > old_width, "{old_width} {new_width}");
    }

    #[test]
    fn side_by_side_rows_preserve_unified_diff_palette() {
        let mut removed = DiffLine::new(DiffLineKind::Removed, Some(1), None, "old value");
        removed.changed_ranges.push(ChangedRange::new(0, 3));
        let mut added = DiffLine::new(DiffLineKind::Added, None, Some(1), "new value");
        added.changed_ranges.push(ChangedRange::new(0, 3));

        let rows = render_side_by_side_preview(
            &[PreviewRow::Line(&removed), PreviewRow::Line(&added)],
            80,
        );
        let rendered = format!("{rows:?}");

        assert!(rendered.contains("Rgb(32, 10, 10)"), "{rendered}");
        assert!(rendered.contains("Rgb(0, 24, 16)"), "{rendered}");
        assert!(rendered.contains("Rgb(50, 14, 14)"), "{rendered}");
        assert!(rendered.contains("Rgb(0, 42, 26)"), "{rendered}");
    }

    #[test]
    fn fitting_diff_lines_do_not_wrap() {
        let content = "01234567890123456789012345678901234567890123456789";
        let line = DiffLine::new(DiffLineKind::Added, None, Some(1), content);
        let card_width = u16::try_from(content.len() + INLINE_DIFF_BODY_CHROME_WIDTH)
            .expect("card width fits u16");

        let rows = render_diff_line(&line, card_width);

        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(line_width(&rows[0]), usize::from(card_width) + 2);
    }

    #[test]
    fn wrapped_diff_lines_keep_a_straight_right_edge() {
        let line = DiffLine::new(
            DiffLineKind::Added,
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
    fn diff_viewer_card_excludes_file_and_hunk_headers() {
        let rows = test_diff_viewer_rows(
            "src/lib.rs",
            "fn main() {\n    let value = 1;\n}\n",
            "fn main() {\n    let value = 2;\n}\n",
            "Diff",
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
        let rows = test_diff_viewer_rows(
            "src/lib.rs",
            "fn main() {\n    let value = 1;\n}\n",
            "fn main() {\n    let value = 2;\n}\n",
            "Diff",
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
        let rows = test_diff_viewer_rows("src/lib.rs", "", "", "Diff", false, 80);
        assert!(!rows.iter().any(is_card_row));
    }

    #[test]
    fn diff_viewer_card_rows_share_one_width_and_never_exceed_available_width() {
        let available_width = 80;
        let rows = test_diff_viewer_rows(
            "src/lib.rs",
            "let value = 1;\n",
            "let value = 2;\n",
            "Diff",
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
    fn diff_viewer_rows_include_headers() {
        let rows = test_diff_viewer_rows(
            "src/lib.rs",
            "let x = 1;\n",
            "let x = 2;\n",
            "Diff",
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
