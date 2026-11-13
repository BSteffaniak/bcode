//! Pull request threads for agents.
//!
//! They are read from the review's snapshot (the threads as the review app last read and placed
//! them, see [`bcode_code_review_models::ReviewConversationSnapshot`]), so listing them never
//! reaches the forge. Listings page and shorten them; one thread reads in full with the diff
//! hunk it was written on.

use super::bounded;
use bcode_code_review_models::{
    DiffSide, PullThreadRecord, RemoteComment, ReviewConversationSnapshot, ThreadPlacement,
};
use serde::{Deserialize, Serialize};

/// Pull request threads listed per call when the caller does not say.
pub const DEFAULT_PULL_THREADS: usize = 50;
/// Pull request threads listed per call at most.
pub const MAX_PULL_THREADS: usize = 100;
/// Comments of a pull request thread a listing shows at most (the first and the latest).
pub const MAX_LISTED_COMMENTS: usize = 4;
/// Bytes of one comment body a listing shows at most.
pub const MAX_LISTED_BODY_BYTES: usize = 1024;
/// Bytes of one comment body one thread shows at most.
pub const MAX_THREAD_BODY_BYTES: usize = 8 * 1024;

/// Where a thread comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadOrigin {
    /// The review's own (draft) threads, stored locally.
    #[default]
    Local,
    /// A pull request's thread on its forge.
    PullRequest,
}

/// One comment of a pull request thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentView {
    /// Author's login.
    pub author: String,
    /// Body (markdown), shortened when long (marked with `…`).
    pub body: String,
    /// When it was written, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    /// Web page (full thread reads only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Where a pull request thread was written, as the forge reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrittenOn {
    /// File path.
    pub path: String,
    /// Side of the lines (`new` or `old`).
    pub side: DiffSide,
    /// Lines as written (`start-end` or `line`); `None` for a comment on the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_lines: Option<String>,
    /// Lines on the pull request's current diff, when the forge still places it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_lines: Option<String>,
    /// Head commit it was written on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The comment is on the file, not on lines.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub file_level: bool,
}

/// A pull request thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullThreadView {
    /// Thread id (pass it to `code_review.get_thread`).
    pub thread_id: String,
    /// Always [`ThreadOrigin::PullRequest`].
    pub origin: ThreadOrigin,
    /// The pull request (`owner/name#number`).
    pub pull_request: String,
    /// The forge (such as GitHub).
    pub provider: String,
    /// Where the review shows it now (`path:line`, `path`, or `the review`).
    pub location: String,
    /// How it got there: `exact`, `moved` with its code, `on_file` (its code is gone), or
    /// `on_layer` (the pull request no longer changes its file).
    pub placement: ThreadPlacement,
    /// Where it was written.
    pub written_on: WrittenOn,
    /// Resolved.
    pub resolved: bool,
    /// The forge considers its code changed since it was written.
    pub outdated: bool,
    /// Everyone who commented, in order of first comment.
    pub authors: Vec<String>,
    /// Comments in order (a listing shows the first and the latest).
    pub comments: Vec<CommentView>,
    /// Comments left out of `comments`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub comments_omitted: usize,
    /// The diff the first comment was written on, ending at its line (full thread reads only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_hunk: Option<String>,
    /// Web page (full thread reads only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A page of the review's pull request threads.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PullThreadsView {
    /// Threads of this page.
    pub threads: Vec<PullThreadView>,
    /// Threads matching the filters, on every page.
    pub total: usize,
    /// Index of this page's first thread.
    pub offset: usize,
    /// `offset` of the next page, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Resolved threads left out (`include_resolved` lists them).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_resolved: usize,
    /// Threads the review app did not keep (past its snapshot bound).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_unread: usize,
    /// When the review app read them, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at_ms: Option<u64>,
    /// Why there are none, when the review app has not read them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// What a listing of pull request threads selects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PullThreadQuery {
    /// List resolved threads too.
    pub include_resolved: bool,
    /// Only threads written on, or shown on, this file.
    pub path: Option<String>,
    /// First thread of the page.
    pub offset: usize,
    /// Threads per page (bounded by [`MAX_PULL_THREADS`]).
    pub limit: Option<usize>,
}

/// Said when the review app has not read the review's pull request threads.
pub const UNREAD_NOTE: &str = "The review app has not read this review's pull request threads yet; \
     they appear here once the review is open in the app. code_review.pull_request reads the pull \
     request itself.";

/// A page of `snapshot`'s threads selected by `query`.
#[must_use]
pub fn page(
    snapshot: Option<&ReviewConversationSnapshot>,
    query: &PullThreadQuery,
) -> PullThreadsView {
    let Some(snapshot) = snapshot else {
        return PullThreadsView::default();
    };
    let mut omitted_resolved = 0;
    let matching: Vec<&PullThreadRecord> = snapshot
        .threads
        .iter()
        .filter(|record| {
            query
                .path
                .as_deref()
                .is_none_or(|path| record.thread.anchor.path == path || shown_on(record, path))
        })
        .filter(|record| {
            let keep = query.include_resolved || !record.thread.resolved;
            if !keep {
                omitted_resolved += 1;
            }
            keep
        })
        .collect();
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PULL_THREADS)
        .clamp(1, MAX_PULL_THREADS);
    let threads: Vec<PullThreadView> = matching
        .iter()
        .skip(query.offset)
        .take(limit)
        .map(|record| view(record, false))
        .collect();
    let end = query.offset + threads.len();
    PullThreadsView {
        threads,
        total: matching.len(),
        offset: query.offset,
        next_offset: (end < matching.len()).then_some(end),
        omitted_resolved,
        omitted_unread: snapshot.omitted,
        read_at_ms: Some(snapshot.read_at_ms),
        note: None,
    }
}

/// The thread `id` of `snapshot`, in full.
#[must_use]
pub fn find(snapshot: &ReviewConversationSnapshot, id: &str) -> Option<PullThreadView> {
    snapshot
        .threads
        .iter()
        .find(|record| record.id == id)
        .map(|record| view(record, true))
}

/// Whether the review shows `record` on the file `path` (its location is `path` or `path:line`).
fn shown_on(record: &PullThreadRecord, path: &str) -> bool {
    record
        .location
        .strip_prefix(path)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
}

fn lines(start: Option<u32>, end: Option<u32>) -> Option<String> {
    let end = end?;
    Some(match start {
        Some(start) if start != end => format!("{start}-{end}"),
        _ => end.to_string(),
    })
}

fn comment(comment: &RemoteComment, max: usize, full: bool) -> CommentView {
    CommentView {
        author: comment.author.login.clone(),
        body: bounded(&comment.body, max),
        at_ms: comment.created_at_ms,
        url: comment.url.clone().filter(|_| full),
    }
}

/// `record` as agents see it: in full (every comment, longer bodies, the diff hunk and links),
/// or shortened for a listing.
fn view(record: &PullThreadRecord, full: bool) -> PullThreadView {
    let thread = &record.thread;
    let anchor = &thread.anchor;
    let mut authors: Vec<String> = Vec::new();
    for comment in &thread.comments {
        if !authors.contains(&comment.author.login) {
            authors.push(comment.author.login.clone());
        }
    }
    let (comments, comments_omitted) = if full {
        (
            thread
                .comments
                .iter()
                .map(|item| comment(item, MAX_THREAD_BODY_BYTES, true))
                .collect(),
            0,
        )
    } else {
        let count = thread.comments.len();
        let omitted = count.saturating_sub(MAX_LISTED_COMMENTS);
        (
            thread
                .comments
                .iter()
                .enumerate()
                .filter(|(index, _)| *index == 0 || *index > omitted)
                .map(|(_, item)| comment(item, MAX_LISTED_BODY_BYTES, false))
                .collect(),
            omitted,
        )
    };
    PullThreadView {
        thread_id: record.id.clone(),
        origin: ThreadOrigin::PullRequest,
        pull_request: format!("{}#{}", record.repository, record.number),
        provider: record.provider_label.clone(),
        location: record.location.clone(),
        placement: record.placement,
        written_on: WrittenOn {
            path: anchor.path.clone(),
            side: anchor.side,
            original_lines: lines(anchor.original_start_line, anchor.original_line),
            current_lines: lines(anchor.start_line, anchor.line),
            commit: anchor.original_commit_oid.clone(),
            file_level: anchor.file_level,
        },
        resolved: thread.resolved,
        outdated: thread.outdated,
        authors,
        comments,
        comments_omitted,
        diff_hunk: anchor.diff_hunk.clone().filter(|_| full),
        url: thread
            .comments
            .first()
            .and_then(|comment| comment.url.clone())
            .filter(|_| full),
    }
}
