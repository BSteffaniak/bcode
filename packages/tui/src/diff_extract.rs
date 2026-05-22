//! Diff summary extraction for TUI transcript/tool state.

use bmux_tui::diff::{DiffFileSummary, DiffLine, DiffLineKind};

const DIFF_CONTEXT_LINES: usize = 3;
const MAX_LCS_CELLS: usize = 40_000;

/// Semantic file-edit content extracted from a filesystem tool request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditTranscript {
    path: String,
    old_text: String,
    new_text: String,
}

impl FileEditTranscript {
    /// Return a summary for this file edit.
    #[must_use]
    pub fn summary(&self) -> DiffFileSummary {
        let (added, removed) = count_changed_diff_lines(&self.diff_lines());
        DiffFileSummary::new(self.path.clone(), added, removed)
    }

    /// Return diff lines for this file edit.
    #[must_use]
    pub fn diff_lines(&self) -> Vec<DiffLine> {
        diff_lines_from_text(&self.path, &self.old_text, &self.new_text)
    }
}

/// Extract a semantic file edit from a filesystem tool request.
pub fn file_edit_from_tool_request(
    tool_name: &str,
    arguments_json: &str,
) -> Option<FileEditTranscript> {
    let normalized_tool = tool_name.replace(['-', '.'], "_").to_ascii_lowercase();
    if !matches!(
        normalized_tool.as_str(),
        "filesystem_edit" | "filesystem_write"
    ) {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(arguments_json).ok()?;
    let path = value
        .get("path")
        .or_else(|| value.get("file_path"))
        .or_else(|| value.get("file"))?
        .as_str()?;
    let old_text = value
        .get("old_text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let new_text = value
        .get("new_text")
        .or_else(|| value.get("contents"))
        .or_else(|| value.get("content"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    Some(FileEditTranscript {
        path: path.to_owned(),
        old_text: old_text.to_owned(),
        new_text: new_text.to_owned(),
    })
}

/// Extract a file diff preview from a filesystem tool request.
pub fn diff_from_tool_request(
    tool_name: &str,
    arguments_json: &str,
) -> Option<(DiffFileSummary, Vec<DiffLine>)> {
    let edit = file_edit_from_tool_request(tool_name, arguments_json)?;
    Some((edit.summary(), edit.diff_lines()))
}

fn diff_lines_from_text(path: &str, old_text: &str, new_text: &str) -> Vec<DiffLine> {
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

    lines.push(DiffLine::new(
        DiffLineKind::HunkHeader,
        None,
        None,
        hunk_header(context_start, old_context_end, new_context_end),
    ));
    push_prefix_context(&mut lines, &old_lines, &new_lines, context_start, prefix);
    push_changed_lines(
        &mut lines,
        &old_lines[prefix..old_change_end],
        &new_lines[prefix..new_change_end],
        prefix,
    );
    push_suffix_context(
        &mut lines,
        &old_lines,
        &new_lines,
        old_change_end,
        new_change_end,
        old_context_end,
        new_context_end,
    );
    lines
}

fn file_headers(path: &str) -> Vec<DiffLine> {
    vec![
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("--- {path}")),
        DiffLine::new(DiffLineKind::FileHeader, None, None, format!("+++ {path}")),
    ]
}

fn push_unchanged_preview(lines: &mut Vec<DiffLine>, old_lines: &[&str]) {
    lines.push(DiffLine::new(
        DiffLineKind::HunkHeader,
        None,
        None,
        "@@ no content changes @@",
    ));
    for (index, line) in old_lines.iter().take(20).enumerate() {
        let line_number = usize_to_u32(index.saturating_add(1));
        lines.push(DiffLine::new(
            DiffLineKind::Context,
            Some(line_number),
            Some(line_number),
            (*line).to_owned(),
        ));
    }
}

fn hunk_header(context_start: usize, old_context_end: usize, new_context_end: usize) -> String {
    format!(
        "@@ -{},{} +{},{} @@",
        context_start.saturating_add(1),
        old_context_end.saturating_sub(context_start),
        context_start.saturating_add(1),
        new_context_end.saturating_sub(context_start)
    )
}

fn push_prefix_context(
    lines: &mut Vec<DiffLine>,
    old_lines: &[&str],
    new_lines: &[&str],
    context_start: usize,
    prefix: usize,
) {
    for index in context_start..prefix {
        push_context_line(lines, old_lines, new_lines, index, index);
    }
}

fn push_suffix_context(
    lines: &mut Vec<DiffLine>,
    old_lines: &[&str],
    new_lines: &[&str],
    old_change_end: usize,
    new_change_end: usize,
    old_context_end: usize,
    new_context_end: usize,
) {
    let count = old_context_end
        .saturating_sub(old_change_end)
        .min(new_context_end.saturating_sub(new_change_end));
    for offset in 0..count {
        push_context_line(
            lines,
            old_lines,
            new_lines,
            old_change_end.saturating_add(offset),
            new_change_end.saturating_add(offset),
        );
    }
}

fn push_context_line(
    lines: &mut Vec<DiffLine>,
    old_lines: &[&str],
    new_lines: &[&str],
    old_index: usize,
    new_index: usize,
) {
    let Some(content) = old_lines
        .get(old_index)
        .or_else(|| new_lines.get(new_index))
    else {
        return;
    };
    lines.push(DiffLine::new(
        DiffLineKind::Context,
        Some(usize_to_u32(old_index.saturating_add(1))),
        Some(usize_to_u32(new_index.saturating_add(1))),
        (*content).to_owned(),
    ));
}

fn push_changed_lines(
    lines: &mut Vec<DiffLine>,
    old_changed: &[&str],
    new_changed: &[&str],
    prefix: usize,
) {
    if old_changed.len().saturating_mul(new_changed.len()) <= MAX_LCS_CELLS {
        push_lcs_changed_lines(lines, old_changed, new_changed, prefix);
    } else {
        push_simple_changed_lines(lines, old_changed, new_changed, prefix);
    }
}

fn push_simple_changed_lines(
    lines: &mut Vec<DiffLine>,
    old_changed: &[&str],
    new_changed: &[&str],
    prefix: usize,
) {
    for (offset, line) in old_changed.iter().enumerate() {
        lines.push(DiffLine::new(
            DiffLineKind::Removed,
            Some(usize_to_u32(
                prefix.saturating_add(offset).saturating_add(1),
            )),
            None,
            (*line).to_owned(),
        ));
    }
    for (offset, line) in new_changed.iter().enumerate() {
        lines.push(DiffLine::new(
            DiffLineKind::Added,
            None,
            Some(usize_to_u32(
                prefix.saturating_add(offset).saturating_add(1),
            )),
            (*line).to_owned(),
        ));
    }
}

fn push_lcs_changed_lines(
    lines: &mut Vec<DiffLine>,
    old_changed: &[&str],
    new_changed: &[&str],
    prefix: usize,
) {
    let table = lcs_table(old_changed, new_changed);
    let mut old_index = 0usize;
    let mut new_index = 0usize;
    while old_index < old_changed.len() || new_index < new_changed.len() {
        if old_index < old_changed.len()
            && new_index < new_changed.len()
            && old_changed[old_index] == new_changed[new_index]
        {
            push_lcs_context_line(lines, old_changed[old_index], prefix, old_index, new_index);
            old_index = old_index.saturating_add(1);
            new_index = new_index.saturating_add(1);
        } else if new_index < new_changed.len()
            && (old_index == old_changed.len()
                || table[old_index][new_index.saturating_add(1)]
                    >= table[old_index.saturating_add(1)][new_index])
        {
            lines.push(DiffLine::new(
                DiffLineKind::Added,
                None,
                Some(usize_to_u32(
                    prefix.saturating_add(new_index).saturating_add(1),
                )),
                new_changed[new_index].to_owned(),
            ));
            new_index = new_index.saturating_add(1);
        } else if old_index < old_changed.len() {
            lines.push(DiffLine::new(
                DiffLineKind::Removed,
                Some(usize_to_u32(
                    prefix.saturating_add(old_index).saturating_add(1),
                )),
                None,
                old_changed[old_index].to_owned(),
            ));
            old_index = old_index.saturating_add(1);
        }
    }
}

fn push_lcs_context_line(
    lines: &mut Vec<DiffLine>,
    content: &str,
    prefix: usize,
    old_offset: usize,
    new_offset: usize,
) {
    lines.push(DiffLine::new(
        DiffLineKind::Context,
        Some(usize_to_u32(
            prefix.saturating_add(old_offset).saturating_add(1),
        )),
        Some(usize_to_u32(
            prefix.saturating_add(new_offset).saturating_add(1),
        )),
        content.to_owned(),
    ));
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

fn count_changed_diff_lines(lines: &[DiffLine]) -> (u32, u32) {
    let added = lines
        .iter()
        .filter(|line| line.kind == DiffLineKind::Added)
        .count();
    let removed = lines
        .iter()
        .filter(|line| line.kind == DiffLineKind::Removed)
        .count();
    (usize_to_u32(added), usize_to_u32(removed))
}

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
