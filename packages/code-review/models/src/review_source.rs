//! Review-source provider interface (`bcode.review_source/v1`) and stack contracts.
//!
//! Provider plugins (for example the GitHub plugin) list pull requests, detect stacks of
//! pull requests, and fetch a pull request's commits into the local repository. The code review
//! plugin stays provider-neutral: a client asks the provider through this interface and passes
//! what it learned to `review.sources.resolve` as [`ReviewSourceProviderResolution`] values,
//! which the code review service turns into revision-pinned sources.
//!
//! Every type here is provider-neutral: no forge-specific identifiers or fields.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Review-source provider service interface.
pub const REVIEW_SOURCE_INTERFACE_ID: &str = "bcode.review_source/v1";

/// Operation that returns a provider's [`ReviewSourceProviderManifest`].
pub const OP_REVIEW_SOURCE_MANIFEST: &str = "review_source.manifest";
/// Operation that lists a repository's open pull requests.
pub const OP_REVIEW_SOURCE_PULLS_LIST: &str = "review_source.pulls.list";
/// Operation that returns several pull requests by number, as they are now.
pub const OP_REVIEW_SOURCE_PULLS_GET: &str = "review_source.pulls.get";
/// Operation that resolves the linear stack containing one pull request.
pub const OP_REVIEW_SOURCE_STACK_RESOLVE: &str = "review_source.stack.resolve";
/// Operation that fetches a pull request's head and base commits into the local repository.
pub const OP_REVIEW_SOURCE_PULL_FETCH: &str = "review_source.pull.fetch";
/// Operation that makes several pull requests' head and base commits local at once.
pub const OP_REVIEW_SOURCE_PULLS_FETCH: &str = "review_source.pulls.fetch";
/// Code review operation that attributes a whole-stack file diff to the stack's layers.
pub const OP_REVIEW_STACK_ATTRIBUTION: &str = "review.stack.attribution";

/// Error code: the repository is not served by this provider (for example another host).
pub const REVIEW_SOURCE_ERROR_UNSUPPORTED_REPOSITORY: &str = "unsupported_repository";
/// Error code: the provider has no credentials for the repository.
pub const REVIEW_SOURCE_ERROR_UNAUTHENTICATED: &str = "unauthenticated";
/// Error code: the requested pull request is not open in the repository.
pub const REVIEW_SOURCE_ERROR_PULL_NOT_FOUND: &str = "pull_not_found";
/// Error code: fetching the pull request's commits failed.
pub const REVIEW_SOURCE_ERROR_FETCH_FAILED: &str = "fetch_failed";
/// Error code: the provider could not be reached or answered with an error.
pub const REVIEW_SOURCE_ERROR_FAILED: &str = "provider_failed";
/// Error code: the caller stopped wanting the answer (the review was closed) before it was done.
pub const REVIEW_SOURCE_ERROR_CANCELLED: &str = "cancelled";
/// Diagnostic code: the provider failed while the client gathered a source's resolution.
pub const REVIEW_DIAGNOSTIC_PROVIDER_FAILED: &str = "provider_failed";

/// Layer limit of a stack when the configuration sets none.
pub const DEFAULT_REVIEW_STACK_MAX_LAYERS: usize = 64;

/// What happens when a stack has more layers than [`ReviewStackLimit::max_layers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackOverLimit {
    /// Resolve every layer and warn; the warning can be dismissed.
    #[default]
    Warn,
    /// Cut the chain at the limit (at the trunk end below the requested pull request, at the
    /// top above it), with a warning in the stack.
    Stop,
}

/// How many layers a resolved stack may have, and what happens beyond that
/// (`[plugins.config."bcode.code_review".stack]`: `max_layers`, `over_limit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewStackLimit {
    /// Layers beyond which [`Self::over_limit`] applies.
    pub max_layers: usize,
    /// Warn or stop beyond [`Self::max_layers`].
    pub over_limit: StackOverLimit,
}

impl Default for ReviewStackLimit {
    fn default() -> Self {
        Self {
            max_layers: DEFAULT_REVIEW_STACK_MAX_LAYERS,
            over_limit: StackOverLimit::default(),
        }
    }
}

impl ReviewStackLimit {
    /// The limit a provider applies while resolving: the configured one when stopping, none when
    /// only warning.
    #[must_use]
    pub const fn resolve_limit(&self) -> Option<usize> {
        match self.over_limit {
            StackOverLimit::Stop => Some(self.max_layers),
            StackOverLimit::Warn => None,
        }
    }

    /// Whether a stack of `layers` layers deserves the over-limit warning.
    #[must_use]
    pub const fn warns_for(&self, layers: usize) -> bool {
        matches!(self.over_limit, StackOverLimit::Warn) && layers > self.max_layers
    }
}

/// Operations a review-source provider supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewSourceCapabilities {
    /// `review_source.pulls.list` is supported.
    #[serde(default)]
    pub pulls_list: bool,
    /// `review_source.stack.resolve` is supported.
    #[serde(default)]
    pub stack_resolve: bool,
    /// `review_source.pull.fetch` is supported.
    #[serde(default)]
    pub pull_fetch: bool,
    /// Operations that take many items in one call.
    #[serde(default)]
    pub batched: ReviewSourceBatchCapabilities,
}

/// Batched operations a review-source provider supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewSourceBatchCapabilities {
    /// `review_source.pulls.fetch` (several pull requests in one call) is supported.
    #[serde(default)]
    pub pulls_fetch: bool,
    /// `review_source.pulls.get` (several pull requests by number in one call) is supported.
    #[serde(default)]
    pub pulls_get: bool,
    /// `review_source.pulls.history` (several pull requests' push history) is supported.
    #[serde(default)]
    pub pulls_history: bool,
}

/// Response payload for `review_source.manifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourceProviderManifest {
    /// Stable provider id, stored in pull-request source kinds (`provider`).
    pub id: String,
    /// Display name.
    pub label: String,
    /// Supported operations.
    #[serde(default)]
    pub capabilities: ReviewSourceCapabilities,
    /// Conversation operations (reviews, threads, comments, checks) it serves.
    #[serde(default)]
    pub conversation: crate::ConversationCapabilities,
}

/// A pull request as a review source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSummary {
    /// Number within the repository.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Author login, when known.
    #[serde(default)]
    pub author: Option<String>,
    /// Branch the changes go into.
    pub base_branch: String,
    /// Branch the changes come from.
    pub head_branch: String,
    /// Commit the head branch points at, as the provider last saw it.
    pub head_oid: String,
    /// Commit the base branch points at, when the provider reports it.
    #[serde(default)]
    pub base_oid: Option<String>,
    /// Whether a review is requested from the current user.
    #[serde(default)]
    pub review_requested_from_me: bool,
    /// Whether the pull request is a draft.
    #[serde(default)]
    pub draft: bool,
    /// Last update time (RFC 3339), when known.
    #[serde(default)]
    pub updated: Option<String>,
    /// Web page, when the provider has one.
    #[serde(default)]
    pub url: Option<String>,
}

/// Request payload for `review_source.pulls.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsListRequest {
    /// Absolute path of the local repository.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Look up the open pull requests this names instead of listing recent ones: a pull request
    /// number (`123`, `#123`) or URL, or an exact head branch name. A repository's recent list
    /// is bounded, so this finds pull requests that are not in it.
    #[serde(default)]
    pub query: Option<String>,
}

/// Request payload for `review_source.pulls.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsGetRequest {
    /// Absolute path of the local repository.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// The pull requests to return.
    pub numbers: Vec<u64>,
    /// Branches whose open pull requests (those with the branch as their base) to report too: a
    /// stack's top branch, to see layers opened on top of it.
    #[serde(default)]
    pub children_of: Vec<String>,
}

/// Response payload for `review_source.pulls.get`: every requested number is in exactly one of
/// the two lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsGetResponse {
    /// Provider repository identifier the pull requests belong to.
    pub repository: String,
    /// The requested pull requests that are open, as they are now.
    pub pulls: Vec<PullRequestSummary>,
    /// The requested numbers that are not open pull requests (closed, merged, or unknown).
    #[serde(default)]
    pub not_open: Vec<u64>,
    /// For each branch of `children_of`, its open pull requests whose head branch is in the
    /// repository itself (a fork's cannot be a stack layer).
    #[serde(default)]
    pub children: std::collections::BTreeMap<String, Vec<PullRequestSummary>>,
}

/// Response payload for `review_source.pulls.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsListResponse {
    /// Provider repository identifier the list belongs to.
    pub repository: String,
    /// The repository's default branch, when known.
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Open pull requests: review requested from me first, then most recently updated.
    pub pulls: Vec<PullRequestSummary>,
}

/// Request payload for `review_source.stack.resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourceStackResolveRequest {
    /// Absolute path of the local repository.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Pull request whose stack is resolved.
    pub number: u64,
    /// Most layers to resolve; the chain is cut beyond it with a warning. `None` resolves the
    /// whole stack.
    #[serde(default)]
    pub max_layers: Option<usize>,
    /// Branches that discovery suggests are in the stack. A provider may look them up together
    /// up front instead of one at a time; hints never decide the stack, so a wrong or missing
    /// hint changes only the cost.
    #[serde(default)]
    pub hints: Vec<String>,
}

/// One layer of a stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStackLayer {
    /// The layer's pull request.
    pub pull: PullRequestSummary,
    /// Pull request number of the layer below; `None` for the bottom layer, which targets the
    /// trunk.
    #[serde(default)]
    pub parent: Option<u64>,
}

/// An open pull request that shares a parent layer with the chain but is not part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStackSibling {
    /// Pull request number of the shared parent layer.
    pub parent: u64,
    /// The sibling pull request.
    pub pull: PullRequestSummary,
}

/// A linear chain of pull requests where each pull request's base branch is the head branch of
/// the one below it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStack {
    /// Branch the bottom layer targets: the default branch for a complete stack.
    pub trunk: String,
    /// Whether `trunk` is the repository's default branch. `false` means the stack is missing a
    /// layer (for example a closed pull request) or targets another long-lived branch.
    pub rooted_at_default_branch: bool,
    /// Layers from the trunk to the top.
    pub layers: Vec<ReviewStackLayer>,
    /// The pull request the stack was resolved for.
    pub requested: u64,
    /// Open pull requests that branch off a layer of the chain without being part of it.
    #[serde(default)]
    pub siblings: Vec<ReviewStackSibling>,
    /// Human-readable notes: missing layers, guarded cycles, ambiguous branches.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl ReviewStack {
    /// Return the layer for pull request `number`.
    #[must_use]
    pub fn layer(&self, number: u64) -> Option<&ReviewStackLayer> {
        self.layers.iter().find(|layer| layer.pull.number == number)
    }

    /// Return the layers from the trunk up to and including pull request `number`.
    #[must_use]
    pub fn layers_through(&self, number: u64) -> Option<&[ReviewStackLayer]> {
        let position = self
            .layers
            .iter()
            .position(|layer| layer.pull.number == number)?;
        Some(&self.layers[..=position])
    }
}

/// Response payload for `review_source.stack.resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourceStackResolveResponse {
    /// Provider repository identifier the stack belongs to.
    pub repository: String,
    /// The resolved stack.
    pub stack: ReviewStack,
}

/// Request payload for `review_source.pull.fetch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullFetchRequest {
    /// Absolute path of the local repository.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Pull request to fetch.
    pub number: u64,
    /// Base branch to fetch with it; `None` uses the pull request's base branch.
    #[serde(default)]
    pub base_branch: Option<String>,
}

/// One pull request of a `review_source.pulls.fetch` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPullFetchTarget {
    /// Pull request number.
    pub number: u64,
    /// Base branch to fetch with it.
    pub base_branch: String,
    /// Head commit the provider reported, when known: a provider may use a local copy instead of
    /// fetching it.
    #[serde(default)]
    pub head_oid: Option<String>,
    /// Base branch commit the provider reported, when known (as `head_oid`).
    #[serde(default)]
    pub base_oid: Option<String>,
}

/// Request payload for `review_source.pulls.fetch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsFetchRequest {
    /// Absolute path of the local repository.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Pull requests to make local.
    pub pulls: Vec<ReviewPullFetchTarget>,
}

/// Response payload for `review_source.pulls.fetch`: one result per requested pull request, in
/// request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsFetchResponse {
    /// Fetch results.
    pub fetched: Vec<ReviewPullFetchResult>,
}

/// Response payload for `review_source.pull.fetch`: the pull request's commits exist locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPullFetchResult {
    /// Pull request number.
    pub number: u64,
    /// Local reference now holding the head commit.
    pub head_ref: String,
    /// Full head commit id.
    pub head_oid: String,
    /// Base branch name on the provider.
    pub base_branch: String,
    /// Local reference now holding the base branch's commit.
    pub base_ref: String,
    /// Full commit id of the base branch.
    pub base_oid: String,
}

/// What the client learned from a provider for one provider-backed workspace source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourceProviderResolution {
    /// Workspace source id.
    pub source_id: String,
    /// Provider answer.
    pub outcome: ReviewSourceProviderOutcome,
}

/// Provider answer for one workspace source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSourceProviderOutcome {
    /// The pull requests were fetched (and, for stack kinds, the stack resolved).
    Resolved {
        /// The stack, for stack-layer and stack sources.
        #[serde(default)]
        stack: Option<ReviewStack>,
        /// Fetch results for every pull request the source needs.
        fetched: Vec<ReviewPullFetchResult>,
    },
    /// The provider failed; the service reports a `provider_failed` diagnostic.
    Failed {
        /// Provider error code.
        code: String,
        /// Human-readable message.
        message: String,
    },
}

/// One layer of a stack pinned to local commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedStackLayer {
    /// Pull request number.
    pub number: u64,
    /// Pull request title.
    pub title: String,
    /// Base commit of the layer's own diff: the merge base of the layer below's head (or the
    /// trunk for the bottom layer) and this layer's head.
    pub base_oid: String,
    /// Head commit of the layer.
    pub head_oid: String,
    /// Branch the pull request goes into, when the provider reported it.
    #[serde(default)]
    pub base_branch: String,
    /// Branch the pull request comes from, when the provider reported it.
    #[serde(default)]
    pub head_branch: String,
}

/// A stack source pinned to local commits, returned by `review.sources.resolve` for stack and
/// stack-layer sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedReviewStack {
    /// Workspace source id.
    pub source_id: String,
    /// Trunk branch name.
    pub trunk: String,
    /// Commit of the trunk branch.
    pub trunk_oid: String,
    /// Merge base of the trunk and the top layer: the base of the whole-stack diff.
    pub base_oid: String,
    /// Layers from the trunk up to the reviewed layer (stack-layer sources) or the stack's top
    /// (stack sources).
    pub layers: Vec<ResolvedStackLayer>,
}

impl ResolvedReviewStack {
    /// Head commit of the top layer, when the stack has layers.
    #[must_use]
    pub fn top_oid(&self) -> Option<&str> {
        self.layers.last().map(|layer| layer.head_oid.as_str())
    }
}

/// Request payload for `review.stack.attribution`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStackAttributionRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// The resolved stack.
    pub stack: ResolvedReviewStack,
    /// File path at the top layer (or the base path for files deleted by the stack).
    pub path: String,
    /// File path at the stack's base when the stack renamed it.
    #[serde(default)]
    pub old_path: Option<String>,
}

/// Inclusive line range of one side of a whole-stack diff introduced by one layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReviewStackLineAttribution {
    /// First line (1-based, inclusive).
    pub start: u32,
    /// Last line (1-based, inclusive).
    pub end: u32,
    /// Pull request number of the layer that introduced (new side) or removed (old side) the
    /// lines.
    pub layer: u64,
    /// Where the range starts in that layer's own diff: on the new side, the line in the
    /// layer's head; on the old side, the line in the head of the layer below (the trunk merge
    /// base for the bottom layer). Consecutive lines of a range are consecutive there too.
    /// Absent in answers from services that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_line: Option<u32>,
}

/// Response payload for `review.stack.attribution`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStackAttributionResponse {
    /// File path.
    pub path: String,
    /// Ranges of the top layer's file (new side of the whole-stack diff) and the layer that last
    /// wrote each. Lines not listed come from the stack's base unchanged.
    pub new_ranges: Vec<ReviewStackLineAttribution>,
    /// Ranges of the base file (old side of the whole-stack diff) and the layer that removed
    /// each. Lines not listed survive to the top layer.
    pub old_ranges: Vec<ReviewStackLineAttribution>,
    /// Whether the file is binary in any layer (no line attribution is possible).
    #[serde(default)]
    pub binary: bool,
}

impl ReviewStackAttributionResponse {
    /// Layer that introduced new-side line `line` and the line's number in that layer's head,
    /// if any (the number is `None` when the service did not report it).
    #[must_use]
    pub fn new_line_origin(&self, line: u32) -> Option<(u64, Option<u32>)> {
        find_range(&self.new_ranges, line).map(|range| (range.layer, range.layer_line_of(line)))
    }

    /// Layer that removed old-side line `line` and the line's number in the head of the layer
    /// below it, if any.
    #[must_use]
    pub fn old_line_origin(&self, line: u32) -> Option<(u64, Option<u32>)> {
        find_range(&self.old_ranges, line).map(|range| (range.layer, range.layer_line_of(line)))
    }

    /// Layer that introduced new-side line `line`, if any.
    #[must_use]
    pub fn new_line_layer(&self, line: u32) -> Option<u64> {
        find_layer(&self.new_ranges, line)
    }

    /// Layer that removed old-side line `line`, if any.
    #[must_use]
    pub fn old_line_layer(&self, line: u32) -> Option<u64> {
        find_layer(&self.old_ranges, line)
    }
}

fn find_layer(ranges: &[ReviewStackLineAttribution], line: u32) -> Option<u64> {
    find_range(ranges, line).map(|range| range.layer)
}

fn find_range(
    ranges: &[ReviewStackLineAttribution],
    line: u32,
) -> Option<&ReviewStackLineAttribution> {
    let index = ranges.partition_point(|range| range.end < line);
    ranges.get(index).filter(|range| range.start <= line)
}

impl ReviewStackLineAttribution {
    /// The number of `line` (inside this range) in the layer's own diff.
    #[must_use]
    pub fn layer_line_of(&self, line: u32) -> Option<u32> {
        let offset = line.checked_sub(self.start)?;
        (line <= self.end)
            .then_some(self.layer_line?)
            .and_then(|start| start.checked_add(offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(number: u64) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: format!("#{number}"),
            author: None,
            base_branch: "main".into(),
            head_branch: format!("b{number}"),
            head_oid: String::new(),
            base_oid: None,
            review_requested_from_me: false,
            draft: false,
            updated: None,
            url: None,
        }
    }

    #[test]
    fn stacks_select_layers_through_a_pull_request() {
        let stack = ReviewStack {
            trunk: "main".into(),
            rooted_at_default_branch: true,
            layers: [1, 2, 3]
                .into_iter()
                .map(|number| ReviewStackLayer {
                    pull: pull(number),
                    parent: (number > 1).then(|| number - 1),
                })
                .collect(),
            requested: 2,
            siblings: Vec::new(),
            warnings: Vec::new(),
        };
        let numbers = |layers: &[ReviewStackLayer]| {
            layers
                .iter()
                .map(|layer| layer.pull.number)
                .collect::<Vec<_>>()
        };
        assert_eq!(numbers(stack.layers_through(2).unwrap()), vec![1, 2]);
        assert!(stack.layers_through(9).is_none());
        assert_eq!(stack.layer(3).unwrap().parent, Some(2));
    }

    #[test]
    fn attribution_looks_up_lines_by_range() {
        let range = |start, end, layer| ReviewStackLineAttribution {
            start,
            end,
            layer,
            layer_line: None,
        };
        let response = ReviewStackAttributionResponse {
            path: "a".into(),
            new_ranges: vec![range(1, 3, 10), range(7, 7, 11)],
            old_ranges: vec![range(4, 5, 11)],
            binary: false,
        };
        assert_eq!(response.new_line_layer(1), Some(10));
        assert_eq!(response.new_line_layer(3), Some(10));
        assert_eq!(response.new_line_layer(4), None);
        assert_eq!(response.new_line_layer(7), Some(11));
        assert_eq!(response.new_line_layer(8), None);
        assert_eq!(response.old_line_layer(5), Some(11));
        assert_eq!(response.old_line_layer(6), None);
    }

    #[test]
    fn provider_outcomes_round_trip_with_kind_tags() {
        let outcome = ReviewSourceProviderOutcome::Failed {
            code: REVIEW_SOURCE_ERROR_UNAUTHENTICATED.into(),
            message: "no token".into(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains(r#""kind":"failed""#), "{json}");
        assert_eq!(
            serde_json::from_str::<ReviewSourceProviderOutcome>(&json).unwrap(),
            outcome
        );
        let manifest: ReviewSourceProviderManifest =
            serde_json::from_str(r#"{"id":"p","label":"P"}"#).unwrap();
        assert_eq!(manifest.capabilities, ReviewSourceCapabilities::default());
    }
}
