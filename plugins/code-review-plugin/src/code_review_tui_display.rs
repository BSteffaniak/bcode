//! Renderer-neutral code review display document construction.

use bcode_syntax_render::{SyntaxHighlighter, SyntaxStyle};

use crate::code_review_tui::{ReviewFile, ReviewLine, ReviewLineKind};

/// Builder for renderer-neutral code review display documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewDisplayBuilder {
    syntax_highlighting: bool,
    syntax_palette: Option<bcode_syntax_render::SyntaxPalette>,
}

impl Default for ReviewDisplayBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewDisplayBuilder {
    /// Create a display document builder with default decorations enabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            syntax_highlighting: true,
            syntax_palette: None,
        }
    }

    /// Enable or disable syntax highlighting decorations.
    #[must_use]
    pub const fn syntax_highlighting(mut self, enabled: bool) -> Self {
        self.syntax_highlighting = enabled;
        self
    }

    /// Set an optional semantic syntax palette.
    #[must_use]
    pub const fn syntax_palette(
        mut self,
        palette: Option<bcode_syntax_render::SyntaxPalette>,
    ) -> Self {
        self.syntax_palette = palette;
        self
    }

    /// Build display rows for a review file.
    #[must_use]
    pub fn build_file(self, file: &ReviewFile) -> ReviewDisplayFile {
        let mut rows = Vec::new();
        let syntax_highlighter = self
            .syntax_palette
            .map_or_else(SyntaxHighlighter::new, SyntaxHighlighter::with_palette);
        let syntax_hint = file.display_path();
        let can_highlight =
            self.syntax_highlighting && syntax_highlighter.can_highlight(syntax_hint);

        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            let heading = hunk.heading.as_deref().unwrap_or_default();
            rows.push(ReviewDisplayRow {
                source: ReviewDisplayRowSource::HunkHeader,
                old_line: None,
                new_line: None,
                segments: vec![ReviewDisplaySegment::new(
                    format!(
                        "@@ hunk {}/{} · old {}..{} · new {}..{}{} @@",
                        hunk_index.saturating_add(1),
                        file.hunks.len(),
                        hunk.old_start,
                        hunk.old_start
                            .saturating_add(hunk.old_count.saturating_sub(1)),
                        hunk.new_start,
                        hunk.new_start
                            .saturating_add(hunk.new_count.saturating_sub(1)),
                        if heading.is_empty() {
                            String::new()
                        } else {
                            format!(" · {heading}")
                        }
                    ),
                    vec![ReviewDisplayTextRole::HunkHeader],
                )],
            });

            let contents = hunk
                .lines
                .iter()
                .map(|line| line.content.as_str())
                .collect::<Vec<_>>();
            let highlighted = if can_highlight {
                highlighted_code_segments_for_lines_with_highlighter(
                    &syntax_highlighter,
                    syntax_hint,
                    &contents,
                )
            } else {
                contents
                    .iter()
                    .map(|content| plain_code_segments(content))
                    .collect()
            };

            rows.extend(hunk.lines.iter().enumerate().map(|(index, line)| {
                let syntax_segments = highlighted
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| plain_code_segments(&line.content));
                display_row_for_review_line(line, syntax_segments)
            }));
        }

        ReviewDisplayFile { rows }
    }
}

/// Renderer-neutral display data for one review file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDisplayFile {
    /// Display rows for this file.
    pub rows: Vec<ReviewDisplayRow>,
}

/// One logical row in a review display document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDisplayRow {
    /// Source row kind.
    pub source: ReviewDisplayRowSource,
    /// Old file line number, when present.
    pub old_line: Option<u32>,
    /// New file line number, when present.
    pub new_line: Option<u32>,
    /// Text segments with semantic roles.
    pub segments: Vec<ReviewDisplaySegment>,
}

/// Source kind for a display row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDisplayRowSource {
    /// Hunk header row.
    HunkHeader,
    /// Context source row.
    Context,
    /// Added source row.
    Added,
    /// Removed source row.
    Removed,
}

impl ReviewDisplayRowSource {
    /// Return the unified diff marker for this row kind.
    #[must_use]
    pub const fn diff_marker(self) -> Option<char> {
        match self {
            Self::HunkHeader => None,
            Self::Context => Some(' '),
            Self::Added => Some('+'),
            Self::Removed => Some('-'),
        }
    }
}

/// Semantic text segment in a display row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDisplaySegment {
    /// Segment text.
    pub text: String,
    /// Semantic roles that apply to this text.
    pub roles: Vec<ReviewDisplayTextRole>,
}

impl ReviewDisplaySegment {
    /// Create a display text segment.
    #[must_use]
    pub const fn new(text: String, roles: Vec<ReviewDisplayTextRole>) -> Self {
        Self { text, roles }
    }
}

/// Renderer-neutral semantic role for a display text segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDisplayTextRole {
    /// Source code text.
    Code,
    /// Syntax-highlighted source text.
    Syntax(SyntaxStyle),
    /// Context line text.
    DiffContext,
    /// Added line text.
    DiffAdded,
    /// Removed line text.
    DiffRemoved,
    /// Hunk header text.
    HunkHeader,
}

/// Add a display role to every segment in a row.
pub fn add_display_role(segments: &mut [ReviewDisplaySegment], role: &ReviewDisplayTextRole) {
    for segment in segments {
        segment.roles.push(role.clone());
    }
}

/// Build syntax-highlighted code segments for multiple lines using one highlighter pass.
#[must_use]
pub fn highlighted_code_segments_for_lines(
    syntax_hint: &str,
    lines: &[&str],
) -> Vec<Vec<ReviewDisplaySegment>> {
    highlighted_code_segments_for_lines_with_highlighter(
        &SyntaxHighlighter::new(),
        syntax_hint,
        lines,
    )
}

fn highlighted_code_segments_for_lines_with_highlighter(
    syntax_highlighter: &SyntaxHighlighter,
    syntax_hint: &str,
    lines: &[&str],
) -> Vec<Vec<ReviewDisplaySegment>> {
    if !syntax_highlighter.can_highlight(syntax_hint) {
        return lines.iter().map(|line| plain_code_segments(line)).collect();
    }
    syntax_highlighter
        .highlight_lines_tokens(syntax_hint, lines)
        .iter()
        .map(|spans| syntax_code_segments(spans))
        .collect()
}

fn display_row_for_review_line(
    line: &ReviewLine,
    mut segments: Vec<ReviewDisplaySegment>,
) -> ReviewDisplayRow {
    let (source, diff_role) = match line.kind {
        ReviewLineKind::Context => (
            ReviewDisplayRowSource::Context,
            ReviewDisplayTextRole::DiffContext,
        ),
        ReviewLineKind::Added => (
            ReviewDisplayRowSource::Added,
            ReviewDisplayTextRole::DiffAdded,
        ),
        ReviewLineKind::Removed => (
            ReviewDisplayRowSource::Removed,
            ReviewDisplayTextRole::DiffRemoved,
        ),
    };

    add_display_role(&mut segments, &diff_role);

    ReviewDisplayRow {
        source,
        old_line: line.old_line,
        new_line: line.new_line,
        segments,
    }
}

fn plain_code_segments(content: &str) -> Vec<ReviewDisplaySegment> {
    vec![ReviewDisplaySegment::new(
        content.to_owned(),
        vec![ReviewDisplayTextRole::Code],
    )]
}

fn syntax_code_segments(spans: &[bcode_syntax_render::SyntaxSpan]) -> Vec<ReviewDisplaySegment> {
    spans
        .iter()
        .map(|span| {
            ReviewDisplaySegment::new(
                span.content.clone(),
                vec![
                    ReviewDisplayTextRole::Code,
                    ReviewDisplayTextRole::Syntax(span.style),
                ],
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ReviewDisplayBuilder, ReviewDisplayRowSource, ReviewDisplayTextRole};
    use crate::code_review_tui::{
        ReviewFile, ReviewFileStatus, ReviewHunk, ReviewLine, ReviewLineKind,
    };

    #[test]
    fn applies_caller_supplied_syntax_palette() {
        let custom = bcode_syntax_render::SyntaxColor::rgb(12, 34, 56);
        let palette = bcode_syntax_render::SyntaxPalette {
            text: custom,
            comment: custom,
            keyword: custom,
            function: custom,
            variable: custom,
            string: custom,
            number: custom,
            type_name: custom,
            operator: custom,
            punctuation: custom,
            heading: custom,
            link: custom,
            raw: custom,
        };
        let file = ReviewFile {
            old_path: Some("src/lib.rs".to_string()),
            new_path: Some("src/lib.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 1,
            deletions: 0,
            hunks: vec![ReviewHunk {
                old_start: 1,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                heading: None,
                lines: vec![ReviewLine {
                    kind: ReviewLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "pub fn demo() {}".to_string(),
                }],
            }],
            is_binary: false,
        };

        let display = ReviewDisplayBuilder::new()
            .syntax_palette(Some(palette))
            .build_file(&file);
        assert!(display.rows[1].segments.iter().all(|segment| {
            segment.roles.iter().any(|role| {
                matches!(
                    role,
                    ReviewDisplayTextRole::Syntax(style)
                        if matches!(
                            style.foreground,
                            bcode_syntax_render::SyntaxColor::Rgb(12, 34, 56)
                        )
                )
            })
        }));
    }

    #[test]
    fn builds_semantic_rows_for_unified_diff() {
        let file = ReviewFile {
            old_path: Some("src/lib.rs".to_string()),
            new_path: Some("src/lib.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 1,
            deletions: 0,
            hunks: vec![ReviewHunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
                heading: Some("fn demo".to_string()),
                lines: vec![ReviewLine {
                    kind: ReviewLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    content: "pub fn demo() {}".to_string(),
                }],
            }],
            is_binary: false,
        };

        let display = ReviewDisplayBuilder::new()
            .syntax_highlighting(true)
            .build_file(&file);

        assert_eq!(display.rows.len(), 2);
        assert_eq!(display.rows[0].source, ReviewDisplayRowSource::HunkHeader);
        assert_eq!(
            display.rows[0].segments[0].text,
            "@@ hunk 1/1 · old 1..1 · new 1..2 · fn demo @@"
        );
        assert_eq!(display.rows[1].source, ReviewDisplayRowSource::Added);
        assert!(
            display.rows[1]
                .segments
                .iter()
                .flat_map(|segment| &segment.roles)
                .any(|role| matches!(role, ReviewDisplayTextRole::Syntax(_)))
        );
    }
}
