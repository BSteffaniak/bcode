//! Transcript-like semantic view document construction for code review panes.

use std::collections::BTreeSet;

use crate::code_review_tui::{CachedReviewFile, ReviewDraftComment, ReviewFile};
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
            .map(|(source_row, display_row)| {
                let target = display_row_target(file_index, source_row, &display_row);
                ReviewViewRow {
                    visual_row: source_row,
                    source_row: Some(source_row),
                    target,
                    block: ReviewViewBlock::DisplayRow(display_row),
                }
            })
            .collect();
        Self { rows }
    }

    /// Build a view document for a materialized full-file surface.
    #[must_use]
    pub fn build_materialized_file_surface(file_index: usize, file: &ReviewFile) -> Self {
        let rows = materialized_file_surface_rows(file)
            .into_iter()
            .enumerate()
            .map(|(source_row, (line_number, content))| ReviewViewRow {
                visual_row: source_row,
                source_row: Some(source_row),
                target: line_number.map_or(
                    ReviewViewTarget::HunkHeader {
                        file_index,
                        source_row,
                    },
                    |line_number| ReviewViewTarget::SourceLine {
                        file_index,
                        source_row,
                        old_line: None,
                        new_line: Some(line_number),
                    },
                ),
                block: ReviewViewBlock::FileLine {
                    line_number,
                    content,
                },
            })
            .collect();
        Self { rows }
    }

    /// Build a view document for a lazily loaded repository file.
    #[must_use]
    pub fn build_repository_file(file_index: usize, file: &CachedReviewFile) -> Self {
        let rows = file
            .line_spans
            .iter()
            .enumerate()
            .filter_map(|(source_row, _)| {
                let content = file.line(source_row)?.to_string();
                let line_number = u32::try_from(source_row.saturating_add(1)).ok();
                Some(ReviewViewRow {
                    visual_row: source_row,
                    source_row: Some(source_row),
                    target: ReviewViewTarget::SourceLine {
                        file_index,
                        source_row,
                        old_line: None,
                        new_line: line_number,
                    },
                    block: ReviewViewBlock::FileLine {
                        line_number,
                        content,
                    },
                })
            })
            .collect();
        Self { rows }
    }

    /// Return a copy with inline draft thread rows inserted after anchor end rows.
    #[must_use]
    pub fn with_inline_draft_threads(
        mut self,
        file_index: usize,
        drafts: impl Iterator<Item = (ReviewThreadAnchor, Vec<ReviewDraftComment>)>,
        collapsed_threads: &BTreeSet<String>,
        resolved_threads: &BTreeSet<String>,
        show_resolved_threads: bool,
    ) -> Self {
        let mut threads = drafts
            .filter(|(anchor, comments)| anchor.file_index == file_index && !comments.is_empty())
            .collect::<Vec<_>>();
        threads.sort_by_key(|(anchor, _)| anchor.end_source_row());

        let mut rows = Vec::with_capacity(self.rows.len().saturating_add(threads.len()));
        for row in self.rows.drain(..) {
            let source_row = row.source_row;
            rows.push(row);
            let Some(source_row) = source_row else {
                continue;
            };
            for (anchor, comments) in threads
                .iter()
                .filter(|(anchor, _)| anchor.end_source_row() == source_row)
            {
                let thread_key = anchor.thread_key();
                let collapsed = collapsed_threads.contains(&thread_key);
                let resolved = resolved_threads.contains(&thread_key);
                if resolved && !show_resolved_threads {
                    continue;
                }
                rows.push(ReviewViewRow {
                    visual_row: 0,
                    source_row: None,
                    target: ReviewViewTarget::Thread {
                        thread_key: thread_key.clone(),
                    },
                    block: ReviewViewBlock::InlineThreadHeader {
                        thread_key: thread_key.clone(),
                        anchor: anchor.clone(),
                        comment_count: comments.len(),
                        collapsed,
                        resolved,
                    },
                });
                if collapsed {
                    continue;
                }
                for (comment_index, comment) in comments.iter().cloned().enumerate() {
                    let body_lines = comment_body_lines(&comment.body);
                    let body_line_count = body_lines.len();
                    for (body_line_index, body_line) in body_lines.into_iter().enumerate() {
                        rows.push(ReviewViewRow {
                            visual_row: 0,
                            source_row: None,
                            target: ReviewViewTarget::Comment {
                                thread_key: thread_key.clone(),
                                comment_index,
                            },
                            block: ReviewViewBlock::InlineComment {
                                thread_key: thread_key.clone(),
                                comment_index,
                                body_line_index,
                                body_line_count,
                                body_line,
                                comment: comment.clone(),
                            },
                        });
                    }
                }
                for action in ReviewThreadAction::all_for_state(resolved) {
                    rows.push(ReviewViewRow {
                        visual_row: 0,
                        source_row: None,
                        target: ReviewViewTarget::ThreadAction {
                            thread_key: thread_key.clone(),
                            action: action.id().to_string(),
                        },
                        block: ReviewViewBlock::InlineThreadAction {
                            thread_key: thread_key.clone(),
                            action,
                        },
                    });
                }
            }
        }
        for (visual_row, row) in rows.iter_mut().enumerate() {
            row.visual_row = visual_row;
        }
        Self { rows }
    }

    /// Return the semantic row for a visual row.
    #[must_use]
    pub fn row_for_visual_row(&self, visual_row: usize) -> Option<&ReviewViewRow> {
        self.rows.get(visual_row)
    }

    /// Return the semantic target for a visual row.
    #[must_use]
    pub fn target_for_visual_row(&self, visual_row: usize) -> Option<&ReviewViewTarget> {
        self.row_for_visual_row(visual_row).map(|row| &row.target)
    }

    /// Return the visual row for a source row.
    #[must_use]
    pub fn visual_row_for_source_row(&self, source_row: usize) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| row.source_row == Some(source_row))
            .map(|row| row.visual_row)
    }
}

/// One semantic row in a review view document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewViewRow {
    /// Zero-based visual row index in this document.
    pub visual_row: usize,
    /// Source/diff row represented by this row, if any.
    pub source_row: Option<usize>,
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
    /// Full-file source line row.
    FileLine {
        /// One-based source line number, when this is source code.
        line_number: Option<u32>,
        /// Line content.
        content: String,
    },
    /// Inline thread header row.
    InlineThreadHeader {
        /// Stable thread key.
        thread_key: String,
        /// Thread anchor.
        anchor: ReviewThreadAnchor,
        /// Number of comments in the thread.
        comment_count: usize,
        /// Whether this thread is collapsed.
        collapsed: bool,
        /// Whether this thread is locally resolved.
        resolved: bool,
    },
    /// Inline comment body row.
    InlineComment {
        /// Stable thread key.
        thread_key: String,
        /// Comment index inside the thread.
        comment_index: usize,
        /// Body line index inside this comment.
        body_line_index: usize,
        /// Total rendered body lines for this comment.
        body_line_count: usize,
        /// Rendered body line.
        body_line: String,
        /// Draft comment body and metadata.
        comment: ReviewDraftComment,
    },
    /// Inline thread action row.
    InlineThreadAction {
        /// Stable thread key.
        thread_key: String,
        /// Action represented by this row.
        action: ReviewThreadAction,
    },
}

/// Inline action exposed for a review thread.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewThreadAction {
    /// Add a reply/draft to the thread.
    Reply,
    /// Edit a draft comment in the thread.
    Edit,
    /// Delete a draft comment in the thread.
    Delete,
    /// Ask Bcode about this thread.
    AskBcode,
    /// Publish review drafts.
    Publish,
    /// Resolve or reopen the thread locally.
    Resolve,
    /// Reopen the thread locally.
    Reopen,
}

impl ReviewThreadAction {
    /// Return all inline thread actions in visual order.
    #[must_use]
    pub const fn all_for_state(resolved: bool) -> [Self; 6] {
        [
            Self::Reply,
            Self::Edit,
            Self::Delete,
            Self::AskBcode,
            Self::Publish,
            if resolved {
                Self::Reopen
            } else {
                Self::Resolve
            },
        ]
    }

    /// Return stable action id.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::AskBcode => "ask",
            Self::Publish => "publish",
            Self::Resolve => "resolve",
            Self::Reopen => "reopen",
        }
    }

    /// Return keyboard shortcut label.
    #[must_use]
    pub const fn shortcut(self) -> &'static str {
        match self {
            Self::Reply => "c",
            Self::Edit => "e",
            Self::Delete => "D",
            Self::AskBcode => "a",
            Self::Publish => "x",
            Self::Resolve | Self::Reopen => "r",
        }
    }

    /// Return display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::AskBcode => "ask Bcode",
            Self::Publish => "publish",
            Self::Resolve => "resolve",
            Self::Reopen => "reopen",
        }
    }

    /// Parse a stable action id.
    #[must_use]
    pub const fn from_id(id: &str) -> Option<Self> {
        match id.as_bytes() {
            b"reply" => Some(Self::Reply),
            b"edit" => Some(Self::Edit),
            b"delete" => Some(Self::Delete),
            b"ask" => Some(Self::AskBcode),
            b"publish" => Some(Self::Publish),
            b"resolve" => Some(Self::Resolve),
            b"reopen" => Some(Self::Reopen),
            _ => None,
        }
    }
}

/// Stable semantic target for selection, mouse, and actions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewViewTarget {
    /// Hunk header row in a diff or materialized file.
    HunkHeader {
        file_index: usize,
        source_row: usize,
    },
    /// Source line row in a diff or file surface.
    SourceLine {
        file_index: usize,
        source_row: usize,
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

/// Minimal, renderer-neutral thread anchor used by view construction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReviewThreadAnchor {
    /// File index in the current review.
    pub file_index: usize,
    /// Display path for the commented file.
    pub path: String,
    /// Source row where the anchor starts.
    pub source_row: usize,
    /// Source row where the anchor ends.
    pub end_source_row: Option<usize>,
}

impl ReviewThreadAnchor {
    /// Return the final source row for this anchor.
    #[must_use]
    pub const fn end_source_row(&self) -> usize {
        match self.end_source_row {
            Some(row) => row,
            None => self.source_row,
        }
    }

    /// Return a stable per-document thread key.
    #[must_use]
    pub fn thread_key(&self) -> String {
        format!(
            "{}:{}:{}-{}",
            self.file_index,
            self.path,
            self.source_row,
            self.end_source_row()
        )
    }
}

const fn display_row_target(
    file_index: usize,
    source_row: usize,
    display_row: &ReviewDisplayRow,
) -> ReviewViewTarget {
    match display_row.source {
        ReviewDisplayRowSource::HunkHeader => ReviewViewTarget::HunkHeader {
            file_index,
            source_row,
        },
        ReviewDisplayRowSource::Context
        | ReviewDisplayRowSource::Added
        | ReviewDisplayRowSource::Removed => ReviewViewTarget::SourceLine {
            file_index,
            source_row,
            old_line: display_row.old_line,
            new_line: display_row.new_line,
        },
    }
}

fn materialized_file_surface_rows(file: &ReviewFile) -> Vec<(Option<u32>, String)> {
    file.hunks
        .iter()
        .flat_map(|hunk| {
            let heading = hunk
                .heading
                .iter()
                .map(|heading| (None, format!("# {heading}")));
            heading.chain(
                hunk.lines
                    .iter()
                    .map(|line| (line.new_line.or(line.old_line), line.content.clone())),
            )
        })
        .collect()
}

fn comment_body_lines(body: &str) -> Vec<String> {
    let lines = body.lines().map(str::to_string).collect::<Vec<_>>();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{ReviewThreadAnchor, ReviewViewBlock, ReviewViewDocument, ReviewViewTarget};
    use crate::code_review_tui::{
        ReviewDraftComment, ReviewFile, ReviewFileStatus, ReviewHunk, ReviewLine, ReviewLineKind,
    };

    #[test]
    fn diff_file_document_maps_visual_rows_to_semantic_targets() {
        let file = test_file();

        let document = ReviewViewDocument::build_diff_file(7, &file, true);

        assert_eq!(document.rows.len(), 2);
        assert_eq!(
            document.target_for_visual_row(0),
            Some(&ReviewViewTarget::HunkHeader {
                file_index: 7,
                source_row: 0,
            })
        );
        assert_eq!(
            document.target_for_visual_row(1),
            Some(&ReviewViewTarget::SourceLine {
                file_index: 7,
                source_row: 1,
                old_line: None,
                new_line: Some(1),
            })
        );
        assert!(matches!(
            document.rows[1].block,
            ReviewViewBlock::DisplayRow(_)
        ));
    }

    #[test]
    fn inline_draft_threads_are_inserted_after_anchor_rows() {
        let file = test_file();
        let anchor = ReviewThreadAnchor {
            file_index: 7,
            path: "src/lib.rs".to_string(),
            source_row: 1,
            end_source_row: None,
        };
        let comment = ReviewDraftComment {
            id: Some("draft-1".to_string()),
            body: "Please double-check this.".to_string(),
            persisted: true,
            created_at_ms: None,
            updated_at_ms: None,
            session_id: None,
        };

        let document = ReviewViewDocument::build_diff_file(7, &file, false)
            .with_inline_draft_threads(
                7,
                std::iter::once((anchor.clone(), vec![comment])),
                &BTreeSet::new(),
                &BTreeSet::new(),
                true,
            );

        assert_eq!(document.rows.len(), 10);
        assert_eq!(document.rows[1].source_row, Some(1));
        assert!(matches!(
            document.rows[2].block,
            ReviewViewBlock::InlineThreadHeader { .. }
        ));
        assert_eq!(
            document.target_for_visual_row(2),
            Some(&ReviewViewTarget::Thread {
                thread_key: anchor.thread_key(),
            })
        );
        assert!(matches!(
            document.rows[3].block,
            ReviewViewBlock::InlineComment { .. }
        ));
        assert!(matches!(
            document.rows[4].block,
            ReviewViewBlock::InlineThreadAction { .. }
        ));
    }

    #[test]
    fn multiline_draft_comments_render_multiple_semantic_rows() {
        let file = test_file();
        let anchor = ReviewThreadAnchor {
            file_index: 7,
            path: "src/lib.rs".to_string(),
            source_row: 1,
            end_source_row: None,
        };
        let comment = ReviewDraftComment {
            id: Some("draft-1".to_string()),
            body: "first\nsecond".to_string(),
            persisted: true,
            created_at_ms: None,
            updated_at_ms: None,
            session_id: None,
        };

        let document = ReviewViewDocument::build_diff_file(7, &file, false)
            .with_inline_draft_threads(
                7,
                std::iter::once((anchor, vec![comment])),
                &BTreeSet::new(),
                &BTreeSet::new(),
                true,
            );

        assert!(matches!(
            document.rows[3].block,
            ReviewViewBlock::InlineComment {
                body_line_index: 0,
                body_line_count: 2,
                ..
            }
        ));
        assert!(matches!(
            document.rows[4].block,
            ReviewViewBlock::InlineComment {
                body_line_index: 1,
                body_line_count: 2,
                ..
            }
        ));
    }

    fn test_file() -> ReviewFile {
        ReviewFile {
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
        }
    }
}
