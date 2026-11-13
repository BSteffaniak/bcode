//! An in-process review-source provider over a fixture repository, for app tests.
//!
//! It serves four open pull requests chained into one stack: #100 (`fixture/base` ← `main`),
//! then #101..#103 for the fixture's stacked branches. Review is requested from me on #102.
//! "Fetching" resolves the local branches, so no network is used.

use super::fixture::{BASE_BRANCH, HEAD_BRANCH, STACK_BRANCHES};
use crate::provider::ProviderCallError;
use bcode_code_review_models::{
    OP_REVIEW_SOURCE_MANIFEST, OP_REVIEW_SOURCE_PULL_FETCH, OP_REVIEW_SOURCE_PULLS_GET,
    OP_REVIEW_SOURCE_PULLS_LIST, OP_REVIEW_SOURCE_STACK_RESOLVE, PullRequestSummary,
    REVIEW_SOURCE_ERROR_PULL_NOT_FOUND, ReviewPullFetchResult, ReviewSourceBatchCapabilities,
    ReviewSourceCapabilities, ReviewSourceProviderManifest, ReviewSourcePullFetchRequest,
    ReviewSourcePullsGetRequest, ReviewSourcePullsGetResponse, ReviewSourcePullsListRequest,
    ReviewSourcePullsListResponse, ReviewSourceStackResolveRequest,
    ReviewSourceStackResolveResponse, ReviewStack, ReviewStackLayer,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// Plugin id of the fake provider.
pub const FAKE_PLUGIN: &str = "test.fake_review_source";
/// Provider id stored in its source kinds.
pub const FAKE_PROVIDER: &str = "fake";
/// Repository identifier it answers for.
pub const FAKE_REPOSITORY: &str = "octo/fixture";
/// Pull request numbers, bottom of the stack first.
pub const FAKE_PULLS: [u64; 4] = [100, 101, 102, 103];
/// The pull request requesting my review.
pub const FAKE_REQUESTED: u64 = 102;

/// The fake provider.
#[derive(Debug)]
pub struct FakeProvider {
    repo: PathBuf,
    pulls_error: Option<ProviderCallError>,
    unlisted: Vec<u64>,
    closed: Mutex<Vec<u64>>,
    calls: Mutex<Vec<String>>,
    conversation: Option<Mutex<FakeConversation>>,
}

/// Conversations the fake serves: kept in memory and changed by the write operations, the
/// viewer being `me`.
#[derive(Debug, Default)]
struct FakeConversation {
    capabilities: bcode_code_review_models::ConversationCapabilities,
    activities: std::collections::BTreeMap<u64, bcode_code_review_models::PullActivity>,
    submitted: Vec<bcode_code_review_models::ReviewSubmitRequest>,
    next_id: u64,
}

impl FakeConversation {
    fn comment(&mut self, body: String) -> bcode_code_review_models::RemoteComment {
        self.next_id += 1;
        bcode_code_review_models::RemoteComment {
            id: format!("posted-{}", self.next_id),
            author: bcode_code_review_models::Participant {
                login: "me".to_owned(),
                is_viewer: true,
                ..Default::default()
            },
            body,
            created_at_ms: Some(2_000_000_000_000 + self.next_id),
            updated_at_ms: None,
            url: None,
            review_id: None,
        }
    }

    fn activity(&mut self, number: u64) -> &mut bcode_code_review_models::PullActivity {
        self.activities
            .entry(number)
            .or_insert_with(|| bcode_code_review_models::PullActivity {
                number,
                title: format!("Layer {number}"),
                head_oid: String::new(),
                state: bcode_code_review_models::PullState::Open,
                author: None,
                description: String::new(),
                url: None,
                decision: None,
                reviews: Vec::new(),
                requested: Vec::new(),
                threads: Vec::new(),
                comments: Vec::new(),
                checks: bcode_code_review_models::Checks::default(),
                viewer: bcode_code_review_models::ViewerAbilities::default(),
                updated_at_ms: None,
            })
    }

    fn thread(
        &mut self,
        number: u64,
        thread_id: &str,
    ) -> &mut bcode_code_review_models::RemoteThread {
        self.activity(number)
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            .expect("known thread")
    }

    /// Preview or submit a review: a submitted one posts its held replies and records its
    /// verdict as the viewer's review.
    fn review(&mut self, operation: &str, payload: &[u8]) -> Vec<u8> {
        use bcode_code_review_models as models;
        let encode = |value: serde_json::Value| serde_json::to_vec(&value).expect("encodes");
        let request: models::ReviewSubmitRequest =
            serde_json::from_slice(payload).expect("review request");
        if operation == models::OP_REVIEW_SOURCE_REVIEW_PREVIEW {
            let comments = request
                .bundle
                .as_ref()
                .map_or(0, |bundle| bundle.threads.len());
            return encode(
                serde_json::to_value(models::ReviewPreviewResponse {
                    preview: format!(
                        "{:?} on #{}: {comments} comments, {} replies",
                        request.verdict,
                        request.pull.number,
                        request.replies.len()
                    ),
                    unplaced: 0,
                })
                .expect("preview"),
            );
        }
        for reply in &request.replies {
            let comment = self.comment(reply.body.clone());
            self.thread(request.pull.number, &reply.thread_id)
                .comments
                .push(comment);
        }
        let review = self.comment(request.body.clone());
        self.activity(request.pull.number)
            .reviews
            .push(models::PullReview {
                id: review.id,
                author: review.author,
                state: match request.verdict {
                    models::ReviewVerdict::Approve => models::ReviewState::Approved,
                    models::ReviewVerdict::RequestChanges => models::ReviewState::ChangesRequested,
                    models::ReviewVerdict::Comment => models::ReviewState::Commented,
                },
                body: request.body.clone(),
                at_ms: review.created_at_ms,
                commit_oid: request.head_oid.clone(),
                url: None,
            });
        let number = request.pull.number;
        self.submitted.push(request);
        encode(
            serde_json::to_value(models::ReviewSubmitResponse {
                url: Some(format!(
                    "https://example.com/{FAKE_REPOSITORY}/pull/{number}"
                )),
                message: format!("Reviewed #{number}"),
            })
            .expect("submit"),
        )
    }

    fn answer(
        &mut self,
        operation: &str,
        payload: &[u8],
        heads: &std::collections::BTreeMap<u64, String>,
    ) -> Vec<u8> {
        use bcode_code_review_models as models;
        let encode = |value: serde_json::Value| serde_json::to_vec(&value).expect("encodes");
        match operation {
            models::OP_REVIEW_SOURCE_PULLS_ACTIVITY => {
                let request: models::ReviewSourcePullsActivityRequest =
                    serde_json::from_slice(payload).expect("activity request");
                let activities = request
                    .numbers
                    .iter()
                    .map(|number| {
                        let mut activity = self.activity(*number).clone();
                        if activity.head_oid.is_empty() {
                            activity.head_oid = heads.get(number).cloned().unwrap_or_default();
                        }
                        activity
                    })
                    .collect();
                encode(
                    serde_json::to_value(models::ReviewSourcePullsActivityResponse {
                        repository: FAKE_REPOSITORY.to_owned(),
                        activities,
                    })
                    .expect("activity"),
                )
            }
            models::OP_REVIEW_SOURCE_THREAD_REPLY => {
                let request: models::ThreadReplyRequest =
                    serde_json::from_slice(payload).expect("reply request");
                let comment = self.comment(request.body);
                self.thread(request.pull.number, &request.thread_id)
                    .comments
                    .push(comment.clone());
                encode(
                    serde_json::to_value(models::CommentPostedResponse { comment }).expect("reply"),
                )
            }
            models::OP_REVIEW_SOURCE_THREAD_RESOLVE => {
                let request: models::ThreadResolveRequest =
                    serde_json::from_slice(payload).expect("resolve request");
                self.thread(request.pull.number, &request.thread_id)
                    .resolved = request.resolved;
                encode(
                    serde_json::to_value(models::ThreadResolveResponse {
                        resolved: request.resolved,
                    })
                    .expect("resolve"),
                )
            }
            models::OP_REVIEW_SOURCE_COMMENT_POST => {
                let request: models::CommentPostRequest =
                    serde_json::from_slice(payload).expect("comment request");
                let comment = self.comment(request.body);
                self.activity(request.pull.number)
                    .comments
                    .push(comment.clone());
                encode(
                    serde_json::to_value(models::CommentPostedResponse { comment })
                        .expect("comment"),
                )
            }
            _ => self.review(operation, payload),
        }
    }
}

impl FakeProvider {
    /// The conversation operations it serves.
    fn conversation_capabilities(&self) -> bcode_code_review_models::ConversationCapabilities {
        self.conversation
            .as_ref()
            .map(|conversation| conversation.lock().expect("conversation lock").capabilities)
            .unwrap_or_default()
    }

    /// Serve pull request conversations with `capabilities` (each pull request starts with an
    /// empty one; see [`Self::set_activity`]).
    #[must_use]
    pub fn with_conversation(
        mut self,
        capabilities: bcode_code_review_models::ConversationCapabilities,
    ) -> Self {
        self.conversation = Some(Mutex::new(FakeConversation {
            capabilities,
            ..FakeConversation::default()
        }));
        self
    }

    /// Replace a pull request's conversation (its head is the branch's current commit when
    /// empty).
    ///
    /// # Panics
    ///
    /// Panics without [`Self::with_conversation`] or when the lock is poisoned.
    pub fn set_activity(&self, activity: bcode_code_review_models::PullActivity) {
        self.conversation
            .as_ref()
            .expect("with_conversation")
            .lock()
            .expect("conversation lock")
            .activities
            .insert(activity.number, activity);
    }

    /// A pull request's conversation as the fake holds it now.
    ///
    /// # Panics
    ///
    /// Panics without [`Self::with_conversation`] or when the lock is poisoned.
    #[must_use]
    pub fn activity(&self, number: u64) -> Option<bcode_code_review_models::PullActivity> {
        self.conversation
            .as_ref()
            .expect("with_conversation")
            .lock()
            .expect("conversation lock")
            .activities
            .get(&number)
            .cloned()
    }

    /// Reviews submitted so far.
    ///
    /// # Panics
    ///
    /// Panics without [`Self::with_conversation`] or when the lock is poisoned.
    #[must_use]
    pub fn submitted(&self) -> Vec<bcode_code_review_models::ReviewSubmitRequest> {
        self.conversation
            .as_ref()
            .expect("with_conversation")
            .lock()
            .expect("conversation lock")
            .submitted
            .clone()
    }

    /// Answer a conversation operation, `None` when `operation` is not one.
    fn converse(
        &self,
        operation: &str,
        payload: &[u8],
    ) -> Option<Result<Vec<u8>, ProviderCallError>> {
        use bcode_code_review_models as models;
        let conversation = self.conversation.as_ref()?;
        let known = [
            models::OP_REVIEW_SOURCE_PULLS_ACTIVITY,
            models::OP_REVIEW_SOURCE_THREAD_REPLY,
            models::OP_REVIEW_SOURCE_THREAD_RESOLVE,
            models::OP_REVIEW_SOURCE_COMMENT_POST,
            models::OP_REVIEW_SOURCE_REVIEW_PREVIEW,
            models::OP_REVIEW_SOURCE_REVIEW_SUBMIT,
        ];
        if !known.contains(&operation) {
            return None;
        }
        self.calls
            .lock()
            .expect("calls lock")
            .push(operation.to_owned());
        let heads: std::collections::BTreeMap<u64, String> = self
            .summaries()
            .into_iter()
            .map(|pull| (pull.number, pull.head_oid))
            .collect();
        let mut state = conversation.lock().expect("conversation lock");
        Some(Ok(state.answer(operation, payload, &heads)))
    }

    /// A provider for the fixture at `repo`.
    /// A provider for the fixture at `repo`.
    #[must_use]
    pub fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            pulls_error: None,
            unlisted: Vec::new(),
            closed: Mutex::default(),
            calls: Mutex::default(),
            conversation: None,
        }
    }

    /// Fail every pull request listing with `code` (for example `unauthenticated`).
    #[must_use]
    pub fn failing_pulls(mut self, code: &str, message: &str) -> Self {
        self.pulls_error = Some(ProviderCallError {
            code: code.to_owned(),
            message: message.to_owned(),
        });
        self
    }

    /// Leave `numbers` out of the recent listing, as a busy repository's older pull requests
    /// are; a lookup by number or head branch still finds them.
    #[must_use]
    pub fn unlisted(mut self, numbers: &[u64]) -> Self {
        self.unlisted = numbers.to_vec();
        self
    }

    /// Close pull request `number` from now on: it leaves listings and the stack.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    pub fn close(&self, number: u64) {
        self.closed.lock().expect("closed lock").push(number);
    }

    /// Open pull request `number` again after [`Self::close`].
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    pub fn reopen(&self, number: u64) {
        self.closed
            .lock()
            .expect("closed lock")
            .retain(|closed| *closed != number);
    }

    /// Answer `review_source.pulls.get`: the open pull requests among the requested numbers.
    fn get(&self, operation: &str, payload: &[u8]) -> Vec<u8> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(operation.to_owned());
        let request: ReviewSourcePullsGetRequest =
            serde_json::from_slice(payload).expect("get request");
        let open = self.summaries();
        let (pulls, not_open): (Vec<_>, Vec<_>) = request
            .numbers
            .iter()
            .map(|number| {
                open.iter()
                    .find(|pull| pull.number == *number)
                    .ok_or(*number)
            })
            .partition(Result::is_ok);
        let children = request
            .children_of
            .iter()
            .map(|branch| {
                let on: Vec<PullRequestSummary> = open
                    .iter()
                    .filter(|pull| pull.base_branch == *branch)
                    .cloned()
                    .collect();
                (branch.clone(), on)
            })
            .collect();
        serde_json::to_vec(&ReviewSourcePullsGetResponse {
            repository: FAKE_REPOSITORY.to_owned(),
            pulls: pulls.into_iter().flatten().cloned().collect(),
            not_open: not_open.into_iter().filter_map(Result::err).collect(),
            children,
        })
        .expect("encodes")
    }

    /// Answer `review_source.pulls.list`: the recent list (without [`Self::unlisted`] pull
    /// requests), or the pull requests a `query` names.
    fn pulls(&self, operation: &str, payload: &[u8]) -> Result<Vec<u8>, ProviderCallError> {
        let record = |entry: String| self.calls.lock().expect("calls lock").push(entry);
        record(operation.to_owned());
        if let Some(error) = &self.pulls_error {
            return Err(error.clone());
        }
        let request: ReviewSourcePullsListRequest =
            serde_json::from_slice(payload).expect("pulls request");
        let mut pulls = self.summaries();
        match request.query.as_deref() {
            Some(query) => {
                record(format!("{operation} {query}"));
                let number = query.strip_prefix('#').unwrap_or(query).parse::<u64>().ok();
                pulls.retain(|pull| Some(pull.number) == number || pull.head_branch == query);
            }
            None => pulls.retain(|pull| !self.unlisted.contains(&pull.number)),
        }
        pulls.sort_by_key(|pull| !pull.review_requested_from_me);
        Ok(serde_json::to_vec(&ReviewSourcePullsListResponse {
            repository: FAKE_REPOSITORY.to_owned(),
            default_branch: Some(BASE_BRANCH.to_owned()),
            pulls,
        })
        .expect("encodes"))
    }

    /// Operations answered so far (`operation` or `operation #number`).
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }

    /// The fixture's pull requests, with the commits their branches point at, as a forge reports
    /// them.
    fn summaries(&self) -> Vec<PullRequestSummary> {
        let closed = self.closed.lock().expect("closed lock").clone();
        let branches = [
            (BASE_BRANCH, HEAD_BRANCH),
            (HEAD_BRANCH, STACK_BRANCHES[0]),
            (STACK_BRANCHES[0], STACK_BRANCHES[1]),
            (STACK_BRANCHES[1], STACK_BRANCHES[2]),
        ];
        FAKE_PULLS
            .iter()
            .zip(branches)
            .map(|(number, (base, head))| PullRequestSummary {
                number: *number,
                title: format!("Layer {number}"),
                author: Some("octocat".to_owned()),
                base_branch: base.to_owned(),
                head_branch: head.to_owned(),
                head_oid: self.rev_parse(head),
                base_oid: Some(self.rev_parse(base)),
                review_requested_from_me: *number == FAKE_REQUESTED,
                draft: false,
                updated: None,
                url: Some(format!(
                    "https://example.com/{FAKE_REPOSITORY}/pull/{number}"
                )),
            })
            .filter(|pull| !closed.contains(&pull.number))
            .collect()
    }

    fn rev_parse(&self, rev: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", rev])
            .current_dir(&self.repo)
            .output()
            .expect("git");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Answer one provider operation.
    ///
    /// # Errors
    ///
    /// The configured listing failure, or `pull_not_found` for unknown numbers.
    ///
    /// # Panics
    ///
    /// Panics when a request does not decode or the lock is poisoned.
    pub fn answer(&self, operation: &str, payload: &[u8]) -> Result<Vec<u8>, ProviderCallError> {
        if let Some(answer) = self.converse(operation, payload) {
            return answer;
        }
        let encode = |value: serde_json::Value| Ok(serde_json::to_vec(&value).expect("encodes"));
        let record = |call: String| self.calls.lock().expect("calls lock").push(call);
        match operation {
            OP_REVIEW_SOURCE_MANIFEST => {
                record(operation.to_owned());
                encode(
                    serde_json::to_value(ReviewSourceProviderManifest {
                        id: FAKE_PROVIDER.to_owned(),
                        label: "Fake forge".to_owned(),
                        capabilities: ReviewSourceCapabilities {
                            pulls_list: true,
                            stack_resolve: true,
                            pull_fetch: true,
                            batched: ReviewSourceBatchCapabilities {
                                pulls_fetch: false,
                                pulls_get: true,
                                pulls_history: false,
                            },
                        },
                        conversation: self.conversation_capabilities(),
                    })
                    .expect("manifest"),
                )
            }
            OP_REVIEW_SOURCE_PULLS_LIST => self.pulls(operation, payload),
            OP_REVIEW_SOURCE_PULLS_GET => Ok(self.get(operation, payload)),
            OP_REVIEW_SOURCE_STACK_RESOLVE => {
                let request: ReviewSourceStackResolveRequest =
                    serde_json::from_slice(payload).expect("stack request");
                // A walk the caller no longer wants stops, as GitHub's does between requests.
                if crate::provider::cancelled() {
                    record(format!("{operation} cancelled"));
                    return Err(ProviderCallError {
                        code: bcode_code_review_models::REVIEW_SOURCE_ERROR_CANCELLED.to_owned(),
                        message: "cancelled".to_owned(),
                    });
                }
                record(format!("{operation} #{}", request.number));
                if !FAKE_PULLS.contains(&request.number) {
                    return Err(ProviderCallError {
                        code: REVIEW_SOURCE_ERROR_PULL_NOT_FOUND.to_owned(),
                        message: format!("no pull request #{}", request.number),
                    });
                }
                let layers = self
                    .summaries()
                    .into_iter()
                    .enumerate()
                    .map(|(index, pull)| ReviewStackLayer {
                        pull,
                        parent: index.checked_sub(1).map(|below| FAKE_PULLS[below]),
                    })
                    .collect();
                encode(
                    serde_json::to_value(ReviewSourceStackResolveResponse {
                        repository: FAKE_REPOSITORY.to_owned(),
                        stack: ReviewStack {
                            trunk: BASE_BRANCH.to_owned(),
                            rooted_at_default_branch: true,
                            layers,
                            requested: request.number,
                            siblings: Vec::new(),
                            warnings: Vec::new(),
                        },
                    })
                    .expect("stack"),
                )
            }
            OP_REVIEW_SOURCE_PULL_FETCH => {
                let request: ReviewSourcePullFetchRequest =
                    serde_json::from_slice(payload).expect("fetch request");
                record(format!("{operation} #{}", request.number));
                let pull = self
                    .summaries()
                    .into_iter()
                    .find(|pull| pull.number == request.number)
                    .ok_or_else(|| ProviderCallError {
                        code: REVIEW_SOURCE_ERROR_PULL_NOT_FOUND.to_owned(),
                        message: format!("no pull request #{}", request.number),
                    })?;
                let base = request.base_branch.unwrap_or(pull.base_branch);
                encode(
                    serde_json::to_value(ReviewPullFetchResult {
                        number: pull.number,
                        head_ref: format!("refs/heads/{}", pull.head_branch),
                        head_oid: self.rev_parse(&pull.head_branch),
                        base_ref: format!("refs/heads/{base}"),
                        base_oid: self.rev_parse(&base),
                        base_branch: base,
                    })
                    .expect("fetch"),
                )
            }
            other => Err(ProviderCallError {
                code: "unknown_operation".to_owned(),
                message: other.to_owned(),
            }),
        }
    }
}
