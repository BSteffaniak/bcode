//! Transcript-like semantic view document construction for code review panes.

use crate::code_review_tui::ReviewFile;
use crate::code_review_tui_display::{
    ReviewDisplayBuilder, ReviewDisplayRow, ReviewDisplayRowSource,
};

/// Semantic document rendered in the main code review pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewViewDocument {
    /// Rows in visual order.
    pub rows: Vec<ReviewViewRow>,
}

impl ReviewViewDocument {
    /// Build a view document for a changed-file diff.
    #[must_use]
    pub fn build_diff_file(
        file_index: usize,
        file: &ReviewFile,
        syntax_highlighting: bool,
    ) -> Self {
        let display = ReviewDisplayBuilder::new()
            .syntax_highlighting(syntax_highlighting)
            .build_file(file);
        let rows = display
            .rows
            .into_iter()
            .enumerate()
            .map(|(render_row, display_row)| {
                let target = match display_row.source {
                    ReviewDisplayRowSource::HunkHeader => ReviewViewTarget::HunkHeader {
                        file_index,
                        diff_row: render_row,
                    },
                    ReviewDisplayRowSource::Context
                    | ReviewDisplayRowSource::Added
                    | ReviewDisplayRowSource::Removed => ReviewViewTarget::SourceLine {
                        file_index,
                        diff_row: render_row,
                        old_line: display_row.old_line,
                        new_line: display_row.new_line,
                    },
                };
                ReviewViewRow {
                    render_row,
                    target,
                    block: ReviewViewBlock::DisplayRow(display_row),
                }
            })
            .collect();
        Self { rows }
    }

    /// Return the semantic target for a rendered row.
    #[must_use]
    pub fn target_for_render_row(&self, render_row: usize) -> Option<&ReviewViewTarget> {
        self.rows
            .get(render_row)
            .filter(|row| row.render_row == render_row)
            .map(|row| &row.target)
    }
}

/// One semantic row in a review view document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewViewRow {
    /// Zero-based rendered row index in this document.
    pub render_row: usize,
    /// Semantic selection/action target represented by this row.
    pub target: ReviewViewTarget,
    /// Renderable semantic block for this row.
    pub block: ReviewViewBlock,
}

/// Semantic row block rendered by the review pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewViewBlock {
    /// Existing source/hunk display row.
    DisplayRow(ReviewDisplayRow),
    /// Placeholder for future inline thread header rows.
    InlineThreadHeader { thread_key: String },
    /// Placeholder for future inline comment body rows.
    InlineComment {
        thread_key: String,
        comment_index: usize,
    },
    /// Placeholder for future inline thread action rows.
    InlineThreadActions { thread_key: String },
}

/// Stable semantic target for selection, mouse, and actions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewViewTarget {
    /// Hunk header row in a diff.
    HunkHeader { file_index: usize, diff_row: usize },
    /// Source line row in a diff or file surface.
    SourceLine {
        file_index: usize,
        diff_row: usize,
        old_line: Option<u32>,
        new_line: Option<u32>,
    },
    /// Inline review thread row.
    Thread { thread_key: String },
    /// Inline review comment row.
    Comment {
        thread_key: String,
        comment_index: usize,
    },
    /// Inline thread action row.
    ThreadAction { thread_key: String, action: String },
}

#[cfg(test)]
mod tests {
    use super::{ReviewViewBlock, ReviewViewDocument, ReviewViewTarget};
    use crate::code_review_tui::{
        ReviewFile, ReviewFileStatus, ReviewHunk, ReviewLine, ReviewLineKind,
    };

    #[test]
    fn diff_file_document_maps_render_rows_to_semantic_targets() {
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
                new_count: 1,
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

        let document = ReviewViewDocument::build_diff_file(7, &file, true);

        assert_eq!(document.rows.len(), 2);
        assert_eq!(
            document.target_for_render_row(0),
            Some(&ReviewViewTarget::HunkHeader {
                file_index: 7,
                diff_row: 0,
            })
        );
        assert_eq!(
            document.target_for_render_row(1),
            Some(&ReviewViewTarget::SourceLine {
                file_index: 7,
                diff_row: 1,
                old_line: None,
                new_line: Some(1),
            })
        );
        assert!(matches!(
            document.rows[1].block,
            ReviewViewBlock::DisplayRow(_)
        ));
    }
}
