//! Transcript-like semantic view document construction for code review panes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use crate::code_review_tui::{
    CachedReviewFile, ReviewAgentThreadState, ReviewDraftComment, ReviewFile,
};
use crate::code_review_tui_display::{
    ReviewDisplayBuilder, ReviewDisplayRow, ReviewDisplayRowSource,
};

const CONTEXT_EXPAND_STEP: u32 = 20;

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
        cached_file: Option<&CachedReviewFile>,
        context_load_states: &BTreeMap<DiffContextKey, DiffContextLoadState>,
    ) -> Self {
        let display = ReviewDisplayBuilder::new()
            .syntax_highlighting(syntax_highlighting)
            .build_file(file);
        let hunk_start_rows = hunk_start_rows(file);
        let display_by_source = display
            .rows
            .into_iter()
            .enumerate()
            .collect::<BTreeMap<_, _>>();
        let mut rows = Vec::with_capacity(display_by_source.len());
        if let Some(first_hunk) = file.hunks.first()
            && first_hunk.new_start > 1
        {
            push_diff_context_rows(
                DiffContextRange {
                    file_index,
                    hunk_index: 0,
                    placement: DiffContextPlacement::Top,
                    start_line: 1,
                    end_line: first_hunk.new_start.saturating_sub(1),
                },
                cached_file,
                context_load_states,
                &mut rows,
            );
        }
        let mut source_row;
        let mut previous_hunk_end_new_line: Option<u32> = None;
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            if let Some(previous_new_line) = previous_hunk_end_new_line
                && hunk.new_start > previous_new_line.saturating_add(1)
            {
                let start_line = previous_new_line.saturating_add(1);
                let end_line = hunk.new_start.saturating_sub(1);
                push_diff_context_rows(
                    DiffContextRange {
                        file_index,
                        hunk_index,
                        placement: DiffContextPlacement::Middle,
                        start_line,
                        end_line,
                    },
                    cached_file,
                    context_load_states,
                    &mut rows,
                );
            }

            let hunk_source_row = hunk_start_rows[hunk_index];
            if let Some(display_row) = display_by_source.get(&hunk_source_row).cloned() {
                rows.push(view_row_for_display(
                    file_index,
                    hunk_source_row,
                    display_row,
                ));
            }
            source_row = hunk_source_row.saturating_add(1);
            for _line in &hunk.lines {
                if let Some(display_row) = display_by_source.get(&source_row).cloned() {
                    rows.push(view_row_for_display(file_index, source_row, display_row));
                }
                source_row = source_row.saturating_add(1);
            }
            previous_hunk_end_new_line = Some(
                hunk.new_start
                    .saturating_add(hunk.new_count)
                    .saturating_sub(1),
            );
        }
        if let (Some(previous_new_line), Some(cached_file)) =
            (previous_hunk_end_new_line, cached_file)
        {
            let total_lines = u32::try_from(cached_file.line_spans.len()).unwrap_or(u32::MAX);
            if total_lines > previous_new_line {
                push_diff_context_rows(
                    DiffContextRange {
                        file_index,
                        hunk_index: file.hunks.len(),
                        placement: DiffContextPlacement::Bottom,
                        start_line: previous_new_line.saturating_add(1),
                        end_line: total_lines,
                    },
                    Some(cached_file),
                    context_load_states,
                    &mut rows,
                );
            }
        }
        renumber_rows(&mut rows);
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
        agent_states: &BTreeMap<String, ReviewAgentThreadState>,
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
                    let body_line_count = comment.body.lines().count().max(1);
                    for body_line_index in 0..body_line_count {
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
                                comment: comment.clone(),
                            },
                        });
                    }
                }
                if let Some(agent_state) = agent_states.get(&thread_key) {
                    let body_line_count = agent_thread_visible_line_count(agent_state);
                    for body_line_index in 0..body_line_count {
                        rows.push(ReviewViewRow {
                            visual_row: 0,
                            source_row: None,
                            target: ReviewViewTarget::AgentThread {
                                thread_key: thread_key.clone(),
                            },
                            block: ReviewViewBlock::InlineAgentThread {
                                thread_key: thread_key.clone(),
                                state: agent_state.clone(),
                                body_line_index,
                                body_line_count,
                            },
                        });
                    }
                }
                let has_agent_answer = agent_states
                    .get(&thread_key)
                    .is_some_and(|state| !state.answer.trim().is_empty());
                let has_linked_session =
                    comments.iter().any(|comment| comment.session_id.is_some())
                        || agent_states
                            .get(&thread_key)
                            .is_some_and(|state| state.session_id.is_some());
                for action in ReviewThreadAction::all_for_state(
                    resolved,
                    has_agent_answer,
                    has_linked_session,
                ) {
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

    /// Return the semantic target for a source row.
    #[must_use]
    pub fn target_for_source_row(&self, source_row: usize) -> Option<&ReviewViewTarget> {
        self.visual_row_for_source_row(source_row)
            .and_then(|visual_row| self.target_for_visual_row(visual_row))
    }

    /// Return the visual row for a source row.
    #[must_use]
    pub fn visual_row_for_source_row(&self, source_row: usize) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| row.source_row == Some(source_row))
            .map(|row| row.visual_row)
    }
    /// Return the visual row for a semantic target.
    #[must_use]
    pub fn visual_row_for_target(&self, target: &ReviewViewTarget) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| &row.target == target)
            .map(|row| row.visual_row)
    }

    /// Return hunk header targets in document order.
    #[must_use]
    pub fn hunk_targets(&self) -> Vec<ReviewViewTarget> {
        self.rows
            .iter()
            .filter_map(|row| match &row.target {
                ReviewViewTarget::HunkHeader { .. } => Some(row.target.clone()),
                _ => None,
            })
            .collect()
    }

    /// Return the next hunk target after a visual row.
    #[must_use]
    pub fn next_hunk_target_after_visual_row(&self, visual_row: usize) -> Option<ReviewViewTarget> {
        self.rows.iter().find_map(|row| {
            (row.visual_row > visual_row
                && matches!(row.target, ReviewViewTarget::HunkHeader { .. }))
            .then(|| row.target.clone())
        })
    }

    /// Return the previous hunk target before a visual row.
    #[must_use]
    pub fn previous_hunk_target_before_visual_row(
        &self,
        visual_row: usize,
    ) -> Option<ReviewViewTarget> {
        self.rows.iter().rev().find_map(|row| {
            (row.visual_row < visual_row
                && matches!(row.target, ReviewViewTarget::HunkHeader { .. }))
            .then(|| row.target.clone())
        })
    }

    /// Return the source row for a semantic target, when applicable.
    #[must_use]
    pub fn source_row_for_target(&self, target: &ReviewViewTarget) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| &row.target == target)
            .and_then(|row| row.source_row)
    }
}

/// One semantic row in a review view document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewViewRow {
    /// Zero-based visual row index in this document.
    pub visual_row: usize,
    /// Commentable source/diff row represented by this row, if any.
    pub source_row: Option<usize>,
    /// Semantic selection/action target represented by this row.
    pub target: ReviewViewTarget,
    /// Renderable semantic block for this row.
    pub block: ReviewViewBlock,
}

/// Semantic row block rendered by the review pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewViewBlock {
    /// Collapsed unchanged diff context row.
    OmittedContext {
        /// Stable expansion key.
        key: DiffContextKey,
        /// Number of hidden lines.
        hidden_line_count: usize,
        /// One-based first hidden new-file line.
        start_line: u32,
        /// One-based final hidden new-file line.
        end_line: u32,
    },
    /// Hidden context is expanded but waiting for full-file content.
    LoadingContext {
        /// Stable expansion key.
        key: DiffContextKey,
        /// Number of hidden lines.
        hidden_line_count: usize,
        /// One-based first hidden new-file line.
        start_line: u32,
        /// One-based final hidden new-file line.
        end_line: u32,
    },
    /// Hidden context expansion failed because full-file content is unavailable.
    UnavailableContext {
        /// Stable expansion key.
        key: DiffContextKey,
        /// Number of hidden lines.
        hidden_line_count: usize,
        /// One-based first hidden new-file line.
        start_line: u32,
        /// One-based final hidden new-file line.
        end_line: u32,
        /// Failure reason.
        reason: String,
    },
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
        /// Total source body lines for this comment.
        body_line_count: usize,
        /// Draft comment body and metadata.
        comment: ReviewDraftComment,
    },
    /// Inline Bcode agent state row.
    InlineAgentThread {
        /// Stable thread key.
        thread_key: String,
        /// Agent state for this thread.
        state: ReviewAgentThreadState,
        /// Body/status line index inside this agent block.
        body_line_index: usize,
        /// Total visible body/status lines for this agent block.
        body_line_count: usize,
    },
    /// Inline thread action row.
    InlineThreadAction {
        /// Stable thread key.
        thread_key: String,
        /// Action represented by this row.
        action: ReviewThreadAction,
    },
}

fn agent_thread_visible_line_count(state: &ReviewAgentThreadState) -> usize {
    let activity_count = usize::from(state.activity.is_some());
    if state.answer.trim().is_empty() {
        1 + activity_count
    } else {
        1 + activity_count + state.answer.lines().count().clamp(1, 4)
    }
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
    /// Ask a follow-up in the linked Bcode session.
    FollowUp,
    /// Convert the latest Bcode answer into a draft comment.
    DraftAnswer,
    /// Open the linked Bcode session.
    OpenSession,
    /// Publish review drafts.
    Publish,
    /// Resolve or reopen the thread locally.
    Resolve,
    /// Reopen the thread locally.
    Reopen,
}

impl ReviewThreadAction {
    /// Return inline thread actions in visual order.
    #[must_use]
    pub fn all_for_state(
        resolved: bool,
        has_agent_answer: bool,
        has_linked_session: bool,
    ) -> Vec<Self> {
        let mut actions = vec![Self::Reply, Self::Edit, Self::Delete];
        if has_linked_session {
            actions.push(Self::FollowUp);
            actions.push(Self::OpenSession);
        } else {
            actions.push(Self::AskBcode);
        }
        if has_agent_answer {
            actions.push(Self::DraftAnswer);
        }
        actions.push(Self::Publish);
        actions.push(if resolved {
            Self::Reopen
        } else {
            Self::Resolve
        });
        actions
    }

    /// Return stable action id.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::AskBcode => "ask",
            Self::FollowUp => "follow-up",
            Self::OpenSession => "open-session",
            Self::DraftAnswer => "draft-answer",
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
            Self::AskBcode | Self::FollowUp => "a",
            Self::OpenSession => "o",
            Self::DraftAnswer => "m",
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
            Self::FollowUp => "ask follow-up",
            Self::OpenSession => "open session",
            Self::DraftAnswer => "draft answer",
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
            b"follow-up" => Some(Self::FollowUp),
            b"open-session" => Some(Self::OpenSession),
            b"draft-answer" => Some(Self::DraftAnswer),
            b"publish" => Some(Self::Publish),
            b"resolve" => Some(Self::Resolve),
            b"reopen" => Some(Self::Reopen),
            _ => None,
        }
    }
}

/// Direction to reveal source lines from a hidden diff context range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiffContextDirection {
    /// Reveal from the start of the hidden range toward later lines.
    Down,
    /// Reveal from the end of the hidden range toward earlier lines.
    Up,
}

/// Typed identity for an expandable hidden diff context block.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiffContextKey {
    /// File index in the current review.
    pub file_index: usize,
    /// Hunk index after the hidden context block.
    pub hunk_index: usize,
    /// One-based first hidden new-file line.
    pub start_line: u32,
    /// One-based final hidden new-file line.
    pub end_line: u32,
    /// Direction to reveal lines from this hidden range.
    pub direction: DiffContextDirection,
}

impl DiffContextKey {
    /// Create a diff context key.
    #[must_use]
    pub const fn new(file_index: usize, hunk_index: usize, start_line: u32, end_line: u32) -> Self {
        Self::with_direction(
            file_index,
            hunk_index,
            start_line,
            end_line,
            DiffContextDirection::Down,
        )
    }

    /// Create a diff context key with an explicit expansion direction.
    #[must_use]
    pub const fn with_direction(
        file_index: usize,
        hunk_index: usize,
        start_line: u32,
        end_line: u32,
        direction: DiffContextDirection,
    ) -> Self {
        Self {
            file_index,
            hunk_index,
            start_line,
            end_line,
            direction,
        }
    }
}

impl fmt::Display for DiffContextKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}:{}-{}:{:?}",
            self.file_index, self.hunk_index, self.start_line, self.end_line, self.direction
        )
    }
}

/// Load state for a typed hidden diff context expansion.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiffContextLoadState {
    /// Full-file content has been requested.
    Loading,
    /// Full-file content is available.
    Loaded,
    /// Full-file content could not be loaded.
    Unavailable(String),
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
    /// Omitted diff context expansion row.
    OmittedContext { key: DiffContextKey },
    /// Expanded hidden context source row.
    ExpandedContextLine {
        key: DiffContextKey,
        line_number: u32,
    },
    /// Inline review thread row.
    Thread { thread_key: String },
    /// Inline review comment row.
    Comment {
        thread_key: String,
        comment_index: usize,
    },
    /// Inline Bcode agent state row.
    AgentThread { thread_key: String },
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

const fn view_row_for_display(
    file_index: usize,
    source_row: usize,
    display_row: ReviewDisplayRow,
) -> ReviewViewRow {
    let target = display_row_target(file_index, source_row, &display_row);
    ReviewViewRow {
        visual_row: source_row,
        source_row: Some(source_row),
        target,
        block: ReviewViewBlock::DisplayRow(display_row),
    }
}

fn renumber_rows(rows: &mut [ReviewViewRow]) {
    for (index, row) in rows.iter_mut().enumerate() {
        row.visual_row = index;
    }
}

fn hunk_start_rows(file: &ReviewFile) -> Vec<usize> {
    let mut rows = Vec::with_capacity(file.hunks.len());
    let mut row = 0usize;
    for hunk in &file.hunks {
        rows.push(row);
        row = row.saturating_add(hunk.lines.len()).saturating_add(1);
    }
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffContextPlacement {
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiffContextRange {
    file_index: usize,
    hunk_index: usize,
    placement: DiffContextPlacement,
    start_line: u32,
    end_line: u32,
}

impl DiffContextRange {
    const fn key(self, direction: DiffContextDirection) -> DiffContextKey {
        DiffContextKey::with_direction(
            self.file_index,
            self.hunk_index,
            self.start_line,
            self.end_line,
            direction,
        )
    }
}

fn push_diff_context_rows(
    range: DiffContextRange,
    cached_file: Option<&CachedReviewFile>,
    context_load_states: &BTreeMap<DiffContextKey, DiffContextLoadState>,
    rows: &mut Vec<ReviewViewRow>,
) {
    let directions = match range.placement {
        DiffContextPlacement::Top => &[DiffContextDirection::Up][..],
        DiffContextPlacement::Middle => {
            if hidden_line_count(range.start_line, range.end_line)
                > usize::try_from(CONTEXT_EXPAND_STEP).unwrap_or(usize::MAX)
            {
                &[DiffContextDirection::Down, DiffContextDirection::Up][..]
            } else {
                &[DiffContextDirection::Down][..]
            }
        }
        DiffContextPlacement::Bottom => &[DiffContextDirection::Down][..],
    };
    for direction in directions {
        push_directed_diff_context_row(range, *direction, cached_file, context_load_states, rows);
    }
}

fn push_directed_diff_context_row(
    range: DiffContextRange,
    direction: DiffContextDirection,
    cached_file: Option<&CachedReviewFile>,
    context_load_states: &BTreeMap<DiffContextKey, DiffContextLoadState>,
    rows: &mut Vec<ReviewViewRow>,
) {
    let key = range.key(direction);
    if let Some(load_state) = context_load_states.get(&key) {
        match (load_state, cached_file) {
            (DiffContextLoadState::Unavailable(reason), _) => rows.push(context_status_row(
                key,
                hidden_line_count(range.start_line, range.end_line),
                range.start_line,
                range.end_line,
                Some(reason.clone()),
            )),
            (DiffContextLoadState::Loaded, Some(file)) if file.unavailable_reason.is_none() => {
                push_incremental_expanded_context_rows(
                    &key,
                    range,
                    file,
                    context_load_states,
                    rows,
                );
            }
            (_, Some(file)) => rows.push(context_status_row(
                key,
                hidden_line_count(range.start_line, range.end_line),
                range.start_line,
                range.end_line,
                file.unavailable_reason.clone(),
            )),
            (DiffContextLoadState::Loading, _) | (_, None) => rows.push(context_status_row(
                key,
                hidden_line_count(range.start_line, range.end_line),
                range.start_line,
                range.end_line,
                None,
            )),
        }
    } else {
        rows.push(omitted_context_row(key, range));
    }
}

fn push_incremental_expanded_context_rows(
    context_key: &DiffContextKey,
    range: DiffContextRange,
    cached_file: &CachedReviewFile,
    context_load_states: &BTreeMap<DiffContextKey, DiffContextLoadState>,
    rows: &mut Vec<ReviewViewRow>,
) {
    match context_key.direction {
        DiffContextDirection::Down => {
            let expanded_end_line = range
                .start_line
                .saturating_add(CONTEXT_EXPAND_STEP.saturating_sub(1))
                .min(range.end_line);
            push_expanded_context_rows(
                context_key,
                cached_file,
                range.start_line,
                expanded_end_line,
                rows,
            );
            let remaining_start_line = expanded_end_line.saturating_add(1);
            if remaining_start_line <= range.end_line {
                push_diff_context_rows(
                    DiffContextRange {
                        start_line: remaining_start_line,
                        ..range
                    },
                    Some(cached_file),
                    context_load_states,
                    rows,
                );
            }
        }
        DiffContextDirection::Up => {
            let expanded_start_line = range
                .end_line
                .saturating_sub(CONTEXT_EXPAND_STEP.saturating_sub(1))
                .max(range.start_line);
            let remaining_end_line = expanded_start_line.saturating_sub(1);
            if range.start_line <= remaining_end_line {
                push_diff_context_rows(
                    DiffContextRange {
                        end_line: remaining_end_line,
                        ..range
                    },
                    Some(cached_file),
                    context_load_states,
                    rows,
                );
            }
            push_expanded_context_rows(
                context_key,
                cached_file,
                expanded_start_line,
                range.end_line,
                rows,
            );
        }
    }
}

fn omitted_context_row(key: DiffContextKey, range: DiffContextRange) -> ReviewViewRow {
    ReviewViewRow {
        visual_row: 0,
        source_row: None,
        target: ReviewViewTarget::OmittedContext { key: key.clone() },
        block: ReviewViewBlock::OmittedContext {
            key,
            hidden_line_count: hidden_line_count(range.start_line, range.end_line),
            start_line: range.start_line,
            end_line: range.end_line,
        },
    }
}

fn hidden_line_count(start_line: u32, end_line: u32) -> usize {
    usize::try_from(end_line.saturating_sub(start_line).saturating_add(1)).unwrap_or(usize::MAX)
}
fn context_status_row(
    key: DiffContextKey,
    hidden_line_count: usize,
    start_line: u32,
    end_line: u32,
    reason: Option<String>,
) -> ReviewViewRow {
    let target = ReviewViewTarget::OmittedContext { key: key.clone() };
    let block = if let Some(reason) = reason {
        ReviewViewBlock::UnavailableContext {
            key,
            hidden_line_count,
            start_line,
            end_line,
            reason,
        }
    } else {
        ReviewViewBlock::LoadingContext {
            key,
            hidden_line_count,
            start_line,
            end_line,
        }
    };
    ReviewViewRow {
        visual_row: 0,
        source_row: None,
        target,
        block,
    }
}

fn push_expanded_context_rows(
    context_key: &DiffContextKey,
    cached_file: &CachedReviewFile,
    start_line: u32,
    end_line: u32,
    rows: &mut Vec<ReviewViewRow>,
) {
    for line_number in start_line..=end_line {
        let Some(source_index) = usize::try_from(line_number.saturating_sub(1)).ok() else {
            continue;
        };
        let Some(content) = cached_file.line(source_index) else {
            continue;
        };
        rows.push(ReviewViewRow {
            visual_row: 0,
            source_row: None,
            target: ReviewViewTarget::ExpandedContextLine {
                key: context_key.clone(),
                line_number,
            },
            block: ReviewViewBlock::FileLine {
                line_number: Some(line_number),
                content: content.to_string(),
            },
        });
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{
        DiffContextDirection, DiffContextKey, DiffContextLoadState, ReviewThreadAnchor,
        ReviewViewBlock, ReviewViewDocument, ReviewViewTarget,
    };
    use crate::code_review_tui::{
        CachedReviewFile, ReviewDraftComment, ReviewFile, ReviewFileStatus, ReviewHunk, ReviewLine,
        ReviewLineKind,
    };
    use bcode_code_review_models::{ReviewThreadKind, ReviewThreadSeverity};

    #[test]
    fn diff_file_document_maps_visual_rows_to_semantic_targets() {
        let file = test_file();

        let document = ReviewViewDocument::build_diff_file(7, &file, true, None, &BTreeMap::new());

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
            thread_kind: ReviewThreadKind::Note,
            severity: ReviewThreadSeverity::Info,
        };

        let document = ReviewViewDocument::build_diff_file(7, &file, false, None, &BTreeMap::new())
            .with_inline_draft_threads(
                7,
                std::iter::once((anchor.clone(), vec![comment])),
                &BTreeMap::new(),
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
            thread_kind: ReviewThreadKind::Note,
            severity: ReviewThreadSeverity::Info,
        };

        let document = ReviewViewDocument::build_diff_file(7, &file, false, None, &BTreeMap::new())
            .with_inline_draft_threads(
                7,
                std::iter::once((anchor, vec![comment])),
                &BTreeMap::new(),
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

    #[test]
    fn expanded_context_rows_use_full_file_source_lines() {
        let file = ReviewFile {
            old_path: Some("src/lib.rs".to_string()),
            new_path: Some("src/lib.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 2,
            deletions: 0,
            hunks: vec![
                ReviewHunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1,
                    heading: Some("first".to_string()),
                    lines: vec![ReviewLine {
                        kind: ReviewLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        content: "pub fn first() {}".to_string(),
                    }],
                },
                ReviewHunk {
                    old_start: 4,
                    old_count: 1,
                    new_start: 4,
                    new_count: 1,
                    heading: Some("second".to_string()),
                    lines: vec![ReviewLine {
                        kind: ReviewLineKind::Added,
                        old_line: None,
                        new_line: Some(4),
                        content: "pub fn second() {}".to_string(),
                    }],
                },
            ],
            is_binary: false,
        };
        let content = "pub fn first() {}\nlet expanded = true;\nlet also_expanded = false;\npub fn second() {}\n".to_string();
        let cached_file = CachedReviewFile {
            path: "src/lib.rs".to_string(),
            line_spans: test_line_spans(&content),
            size_bytes: u64::try_from(content.len()).expect("test content length fits u64"),
            content,
            mtime_ms: None,
            is_binary: false,
            unavailable_reason: None,
        };
        let context_key = DiffContextKey::new(7, 1, 2, 3);
        let mut context_states = BTreeMap::new();
        context_states.insert(context_key, DiffContextLoadState::Loaded);

        let document = ReviewViewDocument::build_diff_file(
            7,
            &file,
            true,
            Some(&cached_file),
            &context_states,
        );

        let expanded_row = document
            .rows
            .iter()
            .find(|row| matches!(row.target, ReviewViewTarget::ExpandedContextLine { .. }))
            .expect("expanded row is rendered");
        let ReviewViewBlock::FileLine {
            line_number,
            content,
        } = &expanded_row.block
        else {
            panic!("expanded row should render as source file line");
        };
        assert_eq!(*line_number, Some(2));
        assert_eq!(content, "let expanded = true;");
    }

    #[test]
    fn diff_file_document_includes_top_and_bottom_hidden_context() {
        let file = ReviewFile {
            old_path: Some("src/lib.rs".to_string()),
            new_path: Some("src/lib.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 1,
            deletions: 0,
            hunks: vec![ReviewHunk {
                old_start: 30,
                old_count: 1,
                new_start: 30,
                new_count: 1,
                heading: Some("middle".to_string()),
                lines: vec![ReviewLine {
                    kind: ReviewLineKind::Added,
                    old_line: None,
                    new_line: Some(30),
                    content: "changed".to_string(),
                }],
            }],
            is_binary: false,
        };
        let content = numbered_lines(60);
        let cached_file = cached_review_file("src/lib.rs", content);

        let document = ReviewViewDocument::build_diff_file(
            7,
            &file,
            false,
            Some(&cached_file),
            &BTreeMap::new(),
        );

        assert!(document.rows.iter().any(|row| matches!(
            &row.block,
            ReviewViewBlock::OmittedContext {
                start_line: 1,
                end_line: 29,
                ..
            }
        )));
        assert!(document.rows.iter().any(|row| matches!(
            &row.block,
            ReviewViewBlock::OmittedContext {
                start_line: 31,
                end_line: 60,
                ..
            }
        )));
    }

    #[test]
    fn expanded_context_reveals_at_most_one_step_and_leaves_remainder() {
        let file = two_hunk_file_with_gap(2, 52);
        let content = numbered_lines(52);
        let cached_file = cached_review_file("src/lib.rs", content);
        let mut context_states = BTreeMap::new();
        context_states.insert(
            DiffContextKey::with_direction(7, 1, 2, 51, DiffContextDirection::Down),
            DiffContextLoadState::Loaded,
        );

        let document = ReviewViewDocument::build_diff_file(
            7,
            &file,
            false,
            Some(&cached_file),
            &context_states,
        );

        let expanded_lines = document
            .rows
            .iter()
            .filter(|row| matches!(row.target, ReviewViewTarget::ExpandedContextLine { .. }))
            .count();
        assert_eq!(expanded_lines, 20);
        assert!(document.rows.iter().any(|row| matches!(
            &row.block,
            ReviewViewBlock::OmittedContext {
                start_line: 22,
                end_line: 51,
                ..
            }
        )));
    }

    #[test]
    fn repeated_context_expansion_reveals_next_step() {
        let file = two_hunk_file_with_gap(2, 52);
        let content = numbered_lines(52);
        let cached_file = cached_review_file("src/lib.rs", content);
        let mut context_states = BTreeMap::new();
        context_states.insert(
            DiffContextKey::with_direction(7, 1, 2, 51, DiffContextDirection::Down),
            DiffContextLoadState::Loaded,
        );
        context_states.insert(
            DiffContextKey::with_direction(7, 1, 22, 51, DiffContextDirection::Down),
            DiffContextLoadState::Loaded,
        );

        let document = ReviewViewDocument::build_diff_file(
            7,
            &file,
            false,
            Some(&cached_file),
            &context_states,
        );

        let expanded_lines = document
            .rows
            .iter()
            .filter_map(|row| match row.target {
                ReviewViewTarget::ExpandedContextLine { line_number, .. } => Some(line_number),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(expanded_lines.len(), 40);
        assert!(expanded_lines.contains(&21));
        assert!(expanded_lines.contains(&41));
        assert!(document.rows.iter().any(|row| matches!(
            &row.block,
            ReviewViewBlock::OmittedContext {
                start_line: 42,
                end_line: 51,
                ..
            }
        )));
    }

    fn numbered_lines(count: u32) -> String {
        (1..=count).fold(String::new(), |mut lines, line| {
            use std::fmt::Write as _;
            writeln!(&mut lines, "line {line}").expect("writing to String cannot fail");
            lines
        })
    }

    fn cached_review_file(path: &str, content: String) -> CachedReviewFile {
        CachedReviewFile {
            path: path.to_string(),
            line_spans: test_line_spans(&content),
            size_bytes: u64::try_from(content.len()).expect("test content length fits u64"),
            content,
            mtime_ms: None,
            is_binary: false,
            unavailable_reason: None,
        }
    }

    fn two_hunk_file_with_gap(start_line: u32, second_hunk_line: u32) -> ReviewFile {
        ReviewFile {
            old_path: Some("src/lib.rs".to_string()),
            new_path: Some("src/lib.rs".to_string()),
            status: ReviewFileStatus::Modified,
            additions: 2,
            deletions: 0,
            hunks: vec![
                ReviewHunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: start_line.saturating_sub(1),
                    new_count: 1,
                    heading: Some("first".to_string()),
                    lines: vec![ReviewLine {
                        kind: ReviewLineKind::Added,
                        old_line: None,
                        new_line: Some(start_line.saturating_sub(1)),
                        content: "first".to_string(),
                    }],
                },
                ReviewHunk {
                    old_start: second_hunk_line,
                    old_count: 1,
                    new_start: second_hunk_line,
                    new_count: 1,
                    heading: Some("second".to_string()),
                    lines: vec![ReviewLine {
                        kind: ReviewLineKind::Added,
                        old_line: None,
                        new_line: Some(second_hunk_line),
                        content: "second".to_string(),
                    }],
                },
            ],
            is_binary: false,
        }
    }

    fn test_line_spans(content: &str) -> Vec<(usize, usize)> {
        content
            .split_inclusive('\n')
            .scan(0usize, |offset, line| {
                let start = *offset;
                *offset = offset.saturating_add(line.len());
                Some((
                    start,
                    start.saturating_add(line.trim_end_matches('\n').len()),
                ))
            })
            .collect()
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
