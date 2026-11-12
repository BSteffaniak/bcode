//! Stable item identities.
//!
//! Keys never contain list indices. A file is identified by source id, part index, and display
//! path; lines by side and line number; hunks by their start lines; threads by id. Unified and
//! side-by-side layouts share every key except the line keys: a unified [`ItemKey::Line`] maps to
//! the [`ItemKey::LinePair`] that contains it and back.

use bcode_code_review_models::{ReviewItemKey, ReviewLineSide};
use std::sync::Arc;

/// Identity of one file of one resolved source (part).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileKey {
    /// Workspace source id.
    pub source_id: String,
    /// Part index for sources that resolve to several parts.
    pub part: Option<u32>,
    /// File display path.
    pub path: String,
}

impl FileKey {
    /// Build a file key.
    #[must_use]
    pub fn new(source_id: impl Into<String>, part: Option<u32>, path: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            part,
            path: path.into(),
        }
    }
}

/// Shared file key; items of one file share one allocation.
pub type FileRef = Arc<FileKey>;

/// Kind of a file notice, part of its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileNoticeKind {
    /// Git treats the content as binary; no lines are shown.
    Binary,
    /// The diff was cut at the hunk line budget.
    TooLarge,
    /// The file's hunks could not be loaded.
    Unavailable,
    /// The file has no textual changes (a pure rename or a mode change).
    NoTextChanges,
}

/// Stable identity of one review document item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ItemKey {
    /// Header of one source (or one part of it).
    SourceHeader {
        /// Workspace source id.
        source_id: String,
        /// Part index.
        part: Option<u32>,
    },
    /// Header of one file.
    FileHeader(FileRef),
    /// Estimated-height placeholder of a file whose content is not resident.
    Placeholder(FileRef),
    /// A notice about a file's content.
    FileNotice {
        /// File.
        file: FileRef,
        /// Notice kind.
        kind: FileNoticeKind,
    },
    /// Header of one hunk.
    HunkHeader {
        /// File.
        file: FileRef,
        /// Old-side start line as reported by git.
        old_start: u32,
        /// New-side start line as reported by git.
        new_start: u32,
    },
    /// One unified line, keyed by the side it belongs to: removed lines by the old side, added,
    /// context, and expanded lines by the new side (the old side for deleted files).
    Line {
        /// File.
        file: FileRef,
        /// Side the line number refers to.
        side: ReviewLineSide,
        /// Line number on that side.
        line: u32,
    },
    /// One side-by-side row.
    LinePair {
        /// File.
        file: FileRef,
        /// Old-side line, when the row has one.
        old: Option<u32>,
        /// New-side line, when the row has one.
        new: Option<u32>,
    },
    /// Unexpanded context, keyed by its first line on the file's context side.
    Gap {
        /// File.
        file: FileRef,
        /// First hidden line on the context side.
        start: u32,
    },
    /// A draft or imported thread.
    Thread {
        /// Thread id.
        thread_id: String,
    },
    /// An AI exchange (or AI suggestions without one).
    AgentThread {
        /// Exchange id, or suggestion id for a suggestion without an exchange.
        id: String,
    },
    /// The inline comment composer (at most one).
    Composer,
}

impl ItemKey {
    /// File this item belongs to, when it belongs to one.
    #[must_use]
    pub const fn file(&self) -> Option<&FileRef> {
        match self {
            Self::FileHeader(file)
            | Self::Placeholder(file)
            | Self::FileNotice { file, .. }
            | Self::HunkHeader { file, .. }
            | Self::Line { file, .. }
            | Self::LinePair { file, .. }
            | Self::Gap { file, .. } => Some(file),
            Self::SourceHeader { .. }
            | Self::Thread { .. }
            | Self::AgentThread { .. }
            | Self::Composer => None,
        }
    }

    /// Whether `self` is a line key that refers to `(side, line)`, directly or as part of a pair.
    #[must_use]
    pub fn covers_line(&self, side: ReviewLineSide, line: u32) -> bool {
        match self {
            Self::Line {
                side: own, line: n, ..
            } => *own == side && *n == line,
            Self::LinePair { old, new, .. } => match side {
                ReviewLineSide::Old => *old == Some(line),
                ReviewLineSide::New => *new == Some(line),
            },
            _ => false,
        }
    }

    /// Convert to the persisted key form, when the key kind can be persisted.
    ///
    /// Side-by-side pairs persist as their new-side line (old side when absent), so a restored
    /// anchor maps back to the same row in either layout.
    #[must_use]
    pub fn to_persisted(&self) -> Option<ReviewItemKey> {
        match self {
            Self::SourceHeader { source_id, .. } => Some(ReviewItemKey::SourceHeader {
                source_id: source_id.clone(),
            }),
            Self::FileHeader(file) | Self::Placeholder(file) | Self::FileNotice { file, .. } => {
                Some(ReviewItemKey::FileHeader {
                    source_id: file.source_id.clone(),
                    path: file.path.clone(),
                    part: file.part,
                })
            }
            Self::HunkHeader {
                file, new_start, ..
            } => Some(ReviewItemKey::HunkHeader {
                source_id: file.source_id.clone(),
                path: file.path.clone(),
                new_start: *new_start,
                part: file.part,
            }),
            Self::Line { file, side, line } => Some(persisted_line(file, *side, *line)),
            Self::LinePair { file, old, new } => match (new, old) {
                (Some(line), _) => Some(persisted_line(file, ReviewLineSide::New, *line)),
                (None, Some(line)) => Some(persisted_line(file, ReviewLineSide::Old, *line)),
                (None, None) => None,
            },
            Self::Thread { thread_id } => Some(ReviewItemKey::Thread {
                thread_id: thread_id.clone(),
            }),
            Self::Gap { .. } | Self::AgentThread { .. } | Self::Composer => None,
        }
    }
}

fn persisted_line(file: &FileKey, side: ReviewLineSide, line: u32) -> ReviewItemKey {
    ReviewItemKey::Line {
        source_id: file.source_id.clone(),
        path: file.path.clone(),
        side,
        line,
        part: file.part,
    }
}
