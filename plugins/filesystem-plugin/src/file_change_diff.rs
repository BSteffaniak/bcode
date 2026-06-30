//! File-change diff extraction for filesystem TUI presentations.

use bcode_syntax_render::{SyntaxHighlighter, SyntaxSpan};
use unicode_segmentation::UnicodeSegmentation;

const DIFF_CONTEXT_LINES: usize = 3;
const MAX_LCS_CELLS: usize = 40_000;
const MAX_INTRALINE_PAIR_CELLS: usize = 10_000;
const MAX_INTRALINE_LINE_GRAPHEMES: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeDiffLineKind {
    FileHeader,
    HunkHeader,
    Context,
    Added,
    Removed,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangeDiffLine {
    pub kind: FileChangeDiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub content: String,
    pub changed_ranges: Vec<ChangedRange>,
    pub syntax_spans: Vec<SyntaxSpan>,
}

impl FileChangeDiffLine {
    #[must_use]
    pub fn new(
        kind: FileChangeDiffLineKind,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangeDiff {
    pub path: String,
    pub lines: Vec<FileChangeDiffLine>,
    pub added: u32,
    pub removed: u32,
}

#[must_use]
pub fn diff_from_text(path: &str, old_text: &str, new_text: &str) -> FileChangeDiff {
    let mut lines = diff_lines_from_text(path, old_text, new_text);
    apply_intraline_changed_ranges(&mut lines);
    apply_syntax_highlighting(path, &mut lines);
    let (added, removed) = count_changed_diff_lines(&lines);
    FileChangeDiff {
        path: path.to_owned(),
        lines,
        added,
        removed,
    }
}

fn diff_lines_from_text(path: &str, old_text: &str, new_text: &str) -> Vec<FileChangeDiffLine> {
    let old_lines = old_text.lines().collect::<Vec<_>>();
    let new_lines = new_text.lines().collect::<Vec<_>>();
    let mut lines = file_headers(path);
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

    lines.push(FileChangeDiffLine::new(
        FileChangeDiffLineKind::HunkHeader,
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
        lines.push(FileChangeDiffLine::new(
            FileChangeDiffLineKind::Context,
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
        lines.push(FileChangeDiffLine::new(
            FileChangeDiffLineKind::Context,
            Some(usize_to_u32(old_index.saturating_add(1))),
            Some(usize_to_u32(new_index.saturating_add(1))),
            old_lines[old_index],
        ));
    }
    lines
}

fn file_headers(path: &str) -> Vec<FileChangeDiffLine> {
    vec![
        FileChangeDiffLine::new(
            FileChangeDiffLineKind::FileHeader,
            None,
            None,
            format!("--- {path}"),
        ),
        FileChangeDiffLine::new(
            FileChangeDiffLineKind::FileHeader,
            None,
            None,
            format!("+++ {path}"),
        ),
    ]
}

fn push_unchanged_preview(lines: &mut Vec<FileChangeDiffLine>, old_lines: &[&str]) {
    let preview_len = old_lines.len().min(DIFF_CONTEXT_LINES.saturating_mul(2));
    for (index, line) in old_lines.iter().take(preview_len).enumerate() {
        let number = usize_to_u32(index.saturating_add(1));
        lines.push(FileChangeDiffLine::new(
            FileChangeDiffLineKind::Context,
            Some(number),
            Some(number),
            *line,
        ));
    }
}

fn push_changed_lines(
    lines: &mut Vec<FileChangeDiffLine>,
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
            lines.push(FileChangeDiffLine::new(
                FileChangeDiffLineKind::Context,
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
            lines.push(FileChangeDiffLine::new(
                FileChangeDiffLineKind::Removed,
                Some(usize_to_u32(
                    base.saturating_add(old_index).saturating_add(1),
                )),
                None,
                old_lines[old_index],
            ));
            old_index = old_index.saturating_add(1);
        } else {
            lines.push(FileChangeDiffLine::new(
                FileChangeDiffLineKind::Added,
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

fn push_removed(lines: &mut Vec<FileChangeDiffLine>, removed: &[&str], base: usize) {
    for (index, line) in removed.iter().enumerate() {
        lines.push(FileChangeDiffLine::new(
            FileChangeDiffLineKind::Removed,
            Some(usize_to_u32(base.saturating_add(index).saturating_add(1))),
            None,
            *line,
        ));
    }
}

fn push_added(lines: &mut Vec<FileChangeDiffLine>, added: &[&str], base: usize) {
    for (index, line) in added.iter().enumerate() {
        lines.push(FileChangeDiffLine::new(
            FileChangeDiffLineKind::Added,
            None,
            Some(usize_to_u32(base.saturating_add(index).saturating_add(1))),
            *line,
        ));
    }
}

fn apply_intraline_changed_ranges(lines: &mut [FileChangeDiffLine]) {
    let mut index = 0usize;
    while index < lines.len() {
        if lines[index].kind != FileChangeDiffLineKind::Removed {
            index = index.saturating_add(1);
            continue;
        }
        let removed_start = index;
        while index < lines.len() && lines[index].kind == FileChangeDiffLineKind::Removed {
            index = index.saturating_add(1);
        }
        let added_start = index;
        while index < lines.len() && lines[index].kind == FileChangeDiffLineKind::Added {
            index = index.saturating_add(1);
        }
        if added_start == index {
            continue;
        }
        apply_changed_ranges_to_run(lines, removed_start, added_start, index);
    }
}

fn apply_changed_ranges_to_run(
    lines: &mut [FileChangeDiffLine],
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
        || old_graphemes.len().saturating_mul(new_graphemes.len()) > MAX_INTRALINE_PAIR_CELLS
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

fn apply_syntax_highlighting(path: &str, lines: &mut [FileChangeDiffLine]) {
    let highlighter = SyntaxHighlighter::new();
    if !highlighter.can_highlight(path) {
        return;
    }
    for line in lines.iter_mut().filter(|line| {
        matches!(
            line.kind,
            FileChangeDiffLineKind::Context
                | FileChangeDiffLineKind::Added
                | FileChangeDiffLineKind::Removed
        )
    }) {
        line.syntax_spans = highlighter.highlight_line_tokens(path, &line.content);
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

fn count_changed_diff_lines(lines: &[FileChangeDiffLine]) -> (u32, u32) {
    (
        usize_to_u32(
            lines
                .iter()
                .filter(|line| line.kind == FileChangeDiffLineKind::Added)
                .count(),
        ),
        usize_to_u32(
            lines
                .iter()
                .filter(|line| line.kind == FileChangeDiffLineKind::Removed)
                .count(),
        ),
    )
}

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
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
                .any(|line| line.kind == FileChangeDiffLineKind::HunkHeader)
        );
    }

    #[test]
    fn changed_ranges_are_unicode_boundary_safe() {
        let diff = diff_from_text("src/lib.rs", "let face = \"😀\";\n", "let face = \"😃\";\n");
        let added = diff
            .lines
            .iter()
            .find(|line| line.kind == FileChangeDiffLineKind::Added)
            .expect("added line");
        assert!(added.changed_ranges.iter().all(|range| {
            added.content.is_char_boundary(range.start) && added.content.is_char_boundary(range.end)
        }));
    }

    #[test]
    fn syntax_highlighting_is_plugin_owned() {
        let diff = diff_from_text("src/main.rs", "", "fn main() {}\n");
        assert!(diff.lines.iter().any(|line| !line.syntax_spans.is_empty()));
    }
}
