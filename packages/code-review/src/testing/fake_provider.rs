//! An in-process review-source provider over a fixture repository, for app tests.
//!
//! It serves four open pull requests chained into one stack: #100 (`fixture/base` ← `main`),
//! then #101..#103 for the fixture's stacked branches. Review is requested from me on #102.
//! "Fetching" resolves the local branches, so no network is used.

use super::fixture::{BASE_BRANCH, HEAD_BRANCH, STACK_BRANCHES};
use crate::provider::ProviderCallError;
use bcode_code_review_models::{
    OP_REVIEW_SOURCE_MANIFEST, OP_REVIEW_SOURCE_PULL_FETCH, OP_REVIEW_SOURCE_PULLS_LIST,
    OP_REVIEW_SOURCE_STACK_RESOLVE, PullRequestSummary, REVIEW_SOURCE_ERROR_PULL_NOT_FOUND,
    ReviewPullFetchResult, ReviewSourceCapabilities, ReviewSourceProviderManifest,
    ReviewSourcePullFetchRequest, ReviewSourcePullsListRequest, ReviewSourcePullsListResponse,
    ReviewSourceStackResolveRequest, ReviewSourceStackResolveResponse, ReviewStack,
    ReviewStackLayer,
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
    calls: Mutex<Vec<String>>,
}

impl FakeProvider {
    /// A provider for the fixture at `repo`.
    #[must_use]
    pub fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            pulls_error: None,
            unlisted: Vec::new(),
            calls: Mutex::default(),
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
        let mut pulls = Self::summaries();
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

    fn summaries() -> Vec<PullRequestSummary> {
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
                head_oid: String::new(),
                base_oid: None,
                review_requested_from_me: *number == FAKE_REQUESTED,
                draft: false,
                updated: None,
                url: Some(format!(
                    "https://example.com/{FAKE_REPOSITORY}/pull/{number}"
                )),
            })
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
                        },
                    })
                    .expect("manifest"),
                )
            }
            OP_REVIEW_SOURCE_PULLS_LIST => self.pulls(operation, payload),
            OP_REVIEW_SOURCE_STACK_RESOLVE => {
                let request: ReviewSourceStackResolveRequest =
                    serde_json::from_slice(payload).expect("stack request");
                record(format!("{operation} #{}", request.number));
                if !FAKE_PULLS.contains(&request.number) {
                    return Err(ProviderCallError {
                        code: REVIEW_SOURCE_ERROR_PULL_NOT_FOUND.to_owned(),
                        message: format!("no pull request #{}", request.number),
                    });
                }
                let layers = Self::summaries()
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
                let pull = Self::summaries()
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
