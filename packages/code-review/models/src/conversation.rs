//! A pull request's conversation as a forge holds it: its state and review decision, reviews,
//! requested reviewers, inline threads, conversation comments, checks, and what the viewer may
//! do. Provider-neutral: providers map their own vocabulary onto these types, and say which
//! operations they serve in [`ConversationCapabilities`]. See
//! `docs/review-forge-conversation.md`.

use crate::ReviewBundle;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Provider operation: several pull requests' conversations in one call.
pub const OP_REVIEW_SOURCE_PULLS_ACTIVITY: &str = "review_source.pulls.activity";
/// Provider operation: wait until some pull requests' conversations change.
pub const OP_REVIEW_SOURCE_PULLS_WATCH: &str = "review_source.pulls.watch";
/// Provider operation: reply to an inline thread now.
pub const OP_REVIEW_SOURCE_THREAD_REPLY: &str = "review_source.thread.reply";
/// Provider operation: resolve or reopen an inline thread.
pub const OP_REVIEW_SOURCE_THREAD_RESOLVE: &str = "review_source.thread.resolve";
/// Provider operation: post a conversation comment.
pub const OP_REVIEW_SOURCE_COMMENT_POST: &str = "review_source.comment.post";
/// Provider operation: show what submitting a review would send.
pub const OP_REVIEW_SOURCE_REVIEW_PREVIEW: &str = "review_source.review.preview";
/// Provider operation: submit a review.
pub const OP_REVIEW_SOURCE_REVIEW_SUBMIT: &str = "review_source.review.submit";

/// Error code: the viewer may not do this (for example approve their own pull request).
pub const REVIEW_SOURCE_ERROR_NOT_PERMITTED: &str = "not_permitted";
/// Error code: the pull request moved since it was read (for example a new head).
pub const REVIEW_SOURCE_ERROR_STALE: &str = "stale";
/// Error code: the forge could not be reached or is rate limiting.
pub const REVIEW_SOURCE_ERROR_UNAVAILABLE: &str = "unavailable";

/// How a provider learns that conversations changed (see [`OP_REVIEW_SOURCE_PULLS_WATCH`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMechanism {
    /// The forge sends events.
    Push,
    /// The provider checks a cheap change marker itself, at a pace the forge allows.
    Conditional,
}

/// The verdicts a provider accepts when submitting a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct VerdictSupport {
    /// A review that only comments.
    #[serde(default)]
    pub comment: bool,
    /// Approval.
    #[serde(default)]
    pub approve: bool,
    /// A request for changes.
    #[serde(default)]
    pub request_changes: bool,
}

impl VerdictSupport {
    /// Whether `verdict` is accepted.
    #[must_use]
    pub const fn supports(self, verdict: ReviewVerdict) -> bool {
        match verdict {
            ReviewVerdict::Comment => self.comment,
            ReviewVerdict::Approve => self.approve,
            ReviewVerdict::RequestChanges => self.request_changes,
        }
    }

    /// Whether any verdict is accepted (the provider submits reviews).
    #[must_use]
    pub const fn any(self) -> bool {
        self.comment || self.approve || self.request_changes
    }

    /// The accepted verdicts, in display order.
    #[must_use]
    pub fn verdicts(self) -> Vec<ReviewVerdict> {
        [
            ReviewVerdict::Comment,
            ReviewVerdict::Approve,
            ReviewVerdict::RequestChanges,
        ]
        .into_iter()
        .filter(|verdict| self.supports(*verdict))
        .collect()
    }
}

/// The conversation operations a provider serves. Everything defaults to unsupported, and the
/// review shows no control for what is unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ConversationCapabilities {
    /// [`OP_REVIEW_SOURCE_PULLS_ACTIVITY`].
    #[serde(default)]
    pub activity: bool,
    /// [`OP_REVIEW_SOURCE_PULLS_WATCH`], and how it learns of changes.
    #[serde(default)]
    pub watch: Option<WatchMechanism>,
    /// [`OP_REVIEW_SOURCE_THREAD_REPLY`].
    #[serde(default)]
    pub thread_reply: bool,
    /// [`OP_REVIEW_SOURCE_THREAD_RESOLVE`].
    #[serde(default)]
    pub thread_resolve: bool,
    /// [`OP_REVIEW_SOURCE_COMMENT_POST`].
    #[serde(default)]
    pub comment_post: bool,
    /// [`OP_REVIEW_SOURCE_REVIEW_PREVIEW`].
    #[serde(default)]
    pub review_preview: bool,
    /// [`OP_REVIEW_SOURCE_REVIEW_SUBMIT`], with the verdicts it accepts.
    #[serde(default)]
    pub verdicts: VerdictSupport,
    /// Replies to existing threads can be held in a submitted review.
    #[serde(default)]
    pub replies_in_review: bool,
}

/// What kind of participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    /// A person.
    #[default]
    User,
    /// A group of people.
    Team,
    /// An automation.
    Bot,
}

/// Someone taking part in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Participant {
    /// Login or handle.
    pub login: String,
    /// Display name, when it differs from the login.
    #[serde(default)]
    pub name: Option<String>,
    /// Kind.
    #[serde(default)]
    pub kind: ParticipantKind,
    /// Web page.
    #[serde(default)]
    pub url: Option<String>,
    /// Whether this is the viewer.
    #[serde(default)]
    pub is_viewer: bool,
}

/// A pull request's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullState {
    /// Open for review.
    #[default]
    Open,
    /// Open, but marked as a draft.
    Draft,
    /// Merged.
    Merged,
    /// Closed without merging.
    Closed,
}

/// Where a pull request's reviews stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// Approved as required.
    Approved,
    /// A reviewer asked for changes.
    ChangesRequested,
    /// A review is required and missing.
    ReviewRequired,
}

/// What a submitted review says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// Comments only.
    Comment,
    /// Approve.
    Approve,
    /// Ask for changes.
    RequestChanges,
}

/// The state of one review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    /// Approved.
    Approved,
    /// Asked for changes.
    ChangesRequested,
    /// Commented.
    Commented,
    /// Dismissed.
    Dismissed,
    /// Started and not submitted (only the viewer's own).
    Pending,
}

/// One review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullReview {
    /// Provider identifier.
    pub id: String,
    /// Reviewer.
    pub author: Participant,
    /// State.
    pub state: ReviewState,
    /// Summary body (markdown), possibly empty.
    #[serde(default)]
    pub body: String,
    /// When it was submitted, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub at_ms: Option<u64>,
    /// The head commit it was given on.
    #[serde(default)]
    pub commit_oid: Option<String>,
    /// Web page.
    #[serde(default)]
    pub url: Option<String>,
}

/// Which side of a diff a line is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// The old side (base).
    Old,
    /// The new side (head).
    #[default]
    New,
}

/// Where an inline thread was written, in the provider's own terms but neutral fields. The
/// review maps it onto its current document; providers do no mapping.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemoteAnchor {
    /// File path (the new path for renames).
    pub path: String,
    /// Side of the line.
    #[serde(default)]
    pub side: DiffSide,
    /// The line on the pull request's current diff, when the forge still places it there.
    #[serde(default)]
    pub line: Option<u32>,
    /// First line of a range on the current diff.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// Side of the range's first line.
    #[serde(default)]
    pub start_side: Option<DiffSide>,
    /// The line as written.
    #[serde(default)]
    pub original_line: Option<u32>,
    /// First line of the range as written.
    #[serde(default)]
    pub original_start_line: Option<u32>,
    /// The head commit the comment was written on.
    #[serde(default)]
    pub original_commit_oid: Option<String>,
    /// The comment is on the file, not a line.
    #[serde(default)]
    pub file_level: bool,
    /// The diff the comment was written on, ending at its line, as the forge shows it (kept
    /// when the thread is outdated). Bounded by the provider; a cut is marked on its first
    /// line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_hunk: Option<String>,
}

/// One comment: on a thread, or in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteComment {
    /// Provider identifier.
    pub id: String,
    /// Author.
    pub author: Participant,
    /// Body (markdown).
    pub body: String,
    /// When written, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub created_at_ms: Option<u64>,
    /// When last edited.
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    /// Web page.
    #[serde(default)]
    pub url: Option<String>,
    /// The review it belongs to, when it was part of one.
    #[serde(default)]
    pub review_id: Option<String>,
}

/// An inline thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct RemoteThread {
    /// Provider identifier (what replies and resolution name).
    pub id: String,
    /// Where it was written.
    pub anchor: RemoteAnchor,
    /// Resolved.
    #[serde(default)]
    pub resolved: bool,
    /// The forge considers its code changed since.
    #[serde(default)]
    pub outdated: bool,
    /// The viewer may reply.
    #[serde(default)]
    pub can_reply: bool,
    /// The viewer may resolve or reopen it.
    #[serde(default)]
    pub can_resolve: bool,
    /// Comments, oldest first.
    pub comments: Vec<RemoteComment>,
}

/// A check's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    /// Failed (or errored).
    Failing,
    /// Queued or running.
    Pending,
    /// Neither passed nor failed (skipped, neutral, cancelled).
    Neutral,
    /// Passed.
    Passing,
}

/// One check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Name.
    pub name: String,
    /// State.
    pub state: CheckState,
    /// Web page.
    #[serde(default)]
    pub url: Option<String>,
}

/// A head commit's checks.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Checks {
    /// Overall state, when the forge reports one.
    #[serde(default)]
    pub state: Option<CheckState>,
    /// Each check, failing first.
    #[serde(default)]
    pub checks: Vec<Check>,
}

impl Checks {
    /// How many checks are in `state`.
    #[must_use]
    pub fn count(&self, state: CheckState) -> usize {
        self.checks
            .iter()
            .filter(|check| check.state == state)
            .count()
    }
}

/// What the viewer may do on a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ViewerAbilities {
    /// The viewer wrote the pull request (and so cannot approve it on most forges).
    #[serde(default)]
    pub is_author: bool,
    /// The viewer may submit reviews.
    #[serde(default)]
    pub can_review: bool,
    /// The viewer may approve or request changes.
    #[serde(default)]
    pub can_judge: bool,
    /// The viewer may comment.
    #[serde(default)]
    pub can_comment: bool,
}

/// One pull request's conversation, as the provider saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullActivity {
    /// Pull request number.
    pub number: u64,
    /// Title.
    #[serde(default)]
    pub title: String,
    /// Head commit when read.
    #[serde(default)]
    pub head_oid: String,
    /// State.
    #[serde(default)]
    pub state: PullState,
    /// Author.
    #[serde(default)]
    pub author: Option<Participant>,
    /// Description (markdown).
    #[serde(default)]
    pub description: String,
    /// Web page.
    #[serde(default)]
    pub url: Option<String>,
    /// Where reviews stand.
    #[serde(default)]
    pub decision: Option<ReviewDecision>,
    /// Reviews, oldest first.
    #[serde(default)]
    pub reviews: Vec<PullReview>,
    /// Reviewers asked for and not yet answered.
    #[serde(default)]
    pub requested: Vec<Participant>,
    /// Inline threads.
    #[serde(default)]
    pub threads: Vec<RemoteThread>,
    /// Conversation comments, oldest first.
    #[serde(default)]
    pub comments: Vec<RemoteComment>,
    /// Head checks.
    #[serde(default)]
    pub checks: Checks,
    /// What the viewer may do.
    #[serde(default)]
    pub viewer: ViewerAbilities,
    /// Last change, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
}

impl PullActivity {
    /// The newest time anything in the conversation happened.
    #[must_use]
    pub fn latest_ms(&self) -> u64 {
        let reviews = self.reviews.iter().filter_map(|review| review.at_ms);
        let comments = self
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .chain(&self.comments)
            .filter_map(|comment| comment.updated_at_ms.or(comment.created_at_ms));
        reviews.chain(comments).max().unwrap_or(0)
    }

    /// Each reviewer's latest review that states a verdict (approved or changes requested),
    /// else their latest, newest reviewer first.
    #[must_use]
    pub fn latest_reviews(&self) -> Vec<&PullReview> {
        let mut latest: Vec<&PullReview> = Vec::new();
        for review in &self.reviews {
            if review.state == ReviewState::Pending {
                continue;
            }
            let judging = matches!(
                review.state,
                ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Dismissed
            );
            match latest
                .iter_mut()
                .find(|kept| kept.author.login == review.author.login)
            {
                Some(kept) => {
                    let kept_judges = matches!(
                        kept.state,
                        ReviewState::Approved
                            | ReviewState::ChangesRequested
                            | ReviewState::Dismissed
                    );
                    if judging || !kept_judges {
                        *kept = review;
                    }
                }
                None => latest.push(review),
            }
        }
        latest.sort_by_key(|review| std::cmp::Reverse(review.at_ms));
        latest
    }

    /// Unresolved inline threads.
    #[must_use]
    pub fn open_threads(&self) -> usize {
        self.threads
            .iter()
            .filter(|thread| !thread.resolved)
            .count()
    }
}

/// Request payload for [`OP_REVIEW_SOURCE_PULLS_ACTIVITY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsActivityRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Pull requests.
    pub numbers: Vec<u64>,
}

/// Response payload for [`OP_REVIEW_SOURCE_PULLS_ACTIVITY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsActivityResponse {
    /// Provider repository identifier.
    pub repository: String,
    /// One per requested pull request the forge knows.
    pub activities: Vec<PullActivity>,
}

/// Request payload for [`OP_REVIEW_SOURCE_PULLS_WATCH`]: wait until a pull request's
/// conversation changes since `cursor`, or `wait_ms` passes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsWatchRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Pull requests to watch.
    pub numbers: Vec<u64>,
    /// What the previous answer returned; `None` starts from now.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Longest wait, in milliseconds.
    pub wait_ms: u64,
    /// Provider-specific settings from the review's configuration, passed through.
    #[serde(default)]
    pub settings: serde_json::Value,
}

/// Response payload for [`OP_REVIEW_SOURCE_PULLS_WATCH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsWatchResponse {
    /// Pull requests whose conversation changed (empty when the wait ran out).
    pub changed: Vec<u64>,
    /// What to pass as the next request's cursor.
    pub cursor: String,
}

/// Names one pull request of a provider's repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullLocator {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Pull request number.
    pub number: u64,
}

/// Request payload for [`OP_REVIEW_SOURCE_THREAD_REPLY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReplyRequest {
    /// The pull request.
    pub pull: PullLocator,
    /// [`RemoteThread::id`].
    pub thread_id: String,
    /// Body (markdown).
    pub body: String,
}

/// Response payload for [`OP_REVIEW_SOURCE_THREAD_REPLY`] and [`OP_REVIEW_SOURCE_COMMENT_POST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentPostedResponse {
    /// The comment as posted.
    pub comment: RemoteComment,
}

/// Request payload for [`OP_REVIEW_SOURCE_THREAD_RESOLVE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadResolveRequest {
    /// The pull request.
    pub pull: PullLocator,
    /// [`RemoteThread::id`].
    pub thread_id: String,
    /// Resolve (`true`) or reopen.
    pub resolved: bool,
}

/// Response payload for [`OP_REVIEW_SOURCE_THREAD_RESOLVE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadResolveResponse {
    /// Whether the thread is resolved now.
    pub resolved: bool,
}

/// Request payload for [`OP_REVIEW_SOURCE_COMMENT_POST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentPostRequest {
    /// The pull request.
    pub pull: PullLocator,
    /// Body (markdown).
    pub body: String,
}

/// A reply to an existing thread held for a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldReply {
    /// [`RemoteThread::id`].
    pub thread_id: String,
    /// Body (markdown).
    pub body: String,
}

/// Request payload for [`OP_REVIEW_SOURCE_REVIEW_PREVIEW`] and [`OP_REVIEW_SOURCE_REVIEW_SUBMIT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSubmitRequest {
    /// The pull request.
    pub pull: PullLocator,
    /// Verdict.
    pub verdict: ReviewVerdict,
    /// Summary body (markdown), possibly empty.
    #[serde(default)]
    pub body: String,
    /// The head the review was made against; providers reject a stale head with
    /// [`REVIEW_SOURCE_ERROR_STALE`] when they can tell.
    #[serde(default)]
    pub head_oid: Option<String>,
    /// New inline comments, with their diff lines (the bundle's threads; summary-only threads
    /// go into the body).
    #[serde(default)]
    pub bundle: Option<ReviewBundle>,
    /// Replies to existing threads held for this review.
    #[serde(default)]
    pub replies: Vec<HeldReply>,
    /// Put comments that cannot be placed inline into the body instead of failing.
    #[serde(default = "default_true")]
    pub unplaced_into_body: bool,
}

const fn default_true() -> bool {
    true
}

/// Response payload for [`OP_REVIEW_SOURCE_REVIEW_PREVIEW`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPreviewResponse {
    /// What would be sent, for a person to read.
    pub preview: String,
    /// Inline comments that would go into the body instead.
    #[serde(default)]
    pub unplaced: usize,
}

/// Response payload for [`OP_REVIEW_SOURCE_REVIEW_SUBMIT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSubmitResponse {
    /// Web page of the review, when there is one.
    #[serde(default)]
    pub url: Option<String>,
    /// For a person to read.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(login: &str, state: ReviewState, at: u64) -> PullReview {
        PullReview {
            id: format!("{login}{at}"),
            author: Participant {
                login: login.to_owned(),
                ..Participant::default()
            },
            state,
            body: String::new(),
            at_ms: Some(at),
            commit_oid: None,
            url: None,
        }
    }

    #[test]
    fn a_later_comment_does_not_hide_a_reviewers_verdict() {
        let activity = PullActivity {
            number: 1,
            title: String::new(),
            head_oid: String::new(),
            state: PullState::Open,
            author: None,
            description: String::new(),
            url: None,
            decision: None,
            reviews: vec![
                review("ana", ReviewState::Approved, 1),
                review("ana", ReviewState::Commented, 2),
                review("raj", ReviewState::Commented, 3),
                review("raj", ReviewState::ChangesRequested, 4),
                review("li", ReviewState::Pending, 5),
            ],
            requested: Vec::new(),
            threads: Vec::new(),
            comments: Vec::new(),
            checks: Checks::default(),
            viewer: ViewerAbilities::default(),
            updated_at_ms: None,
        };
        let latest: Vec<(&str, ReviewState)> = activity
            .latest_reviews()
            .into_iter()
            .map(|review| (review.author.login.as_str(), review.state))
            .collect();
        assert_eq!(
            latest,
            vec![
                ("raj", ReviewState::ChangesRequested),
                ("ana", ReviewState::Approved)
            ]
        );
        assert_eq!(activity.latest_ms(), 5);
    }

    #[test]
    fn capabilities_default_to_nothing_and_read_old_manifests() {
        let capabilities: ConversationCapabilities = serde_json::from_str("{}").expect("parse");
        assert_eq!(capabilities, ConversationCapabilities::default());
        assert!(!capabilities.verdicts.any());
        let verdicts = VerdictSupport {
            comment: true,
            approve: true,
            request_changes: false,
        };
        assert_eq!(
            verdicts.verdicts(),
            vec![ReviewVerdict::Comment, ReviewVerdict::Approve]
        );
    }
}
