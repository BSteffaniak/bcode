//! Path-only discovery contract for `bcode.filesystem/v1`.
//!
//! No filesystem implementation, authorization or discovery runs in this module.
//! Unknown request fields fail closed. Grants are host-owned, not request data.

use serde::{Deserialize, Serialize};

/// Versioned filesystem service identity.
pub const FILESYSTEM_INTERFACE_ID: &str = "bcode.filesystem/v1";
/// Path-only workspace discovery operation.
pub const OP_SEARCH_PATHS: &str = "search_paths";

/// Path search request. Unknown fields are rejected; roots/grants are never accepted here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchPathsRequest {
    /// Case-insensitive Unicode subsequence matched against workspace-relative paths.
    pub query: String,
    /// Git-index tracked paths by default; workspace also includes untracked paths.
    pub scope: SearchPathsScope,
    /// Bypass a fresh inventory and coalesce a bounded refresh.
    pub refresh: bool,
    /// Workspace-relative directory; empty means workspace root. No `..` or absolute paths.
    pub directory: String,
    /// Recurse beneath `directory` when true; false returns its immediate children.
    pub recursive: bool,
    /// Include dot-prefixed names, except `.git`, when host policy permits it.
    pub include_hidden: bool,
    /// Result bound, clamped to `1..=256`.
    pub max_results: usize,
    /// Enumeration bound, clamped to `1..=250_000`, including excluded names.
    pub max_entries: usize,
    /// Directory descent bound, clamped to `1..=64`.
    pub max_depth: usize,
    /// Cooperative traversal deadline, clamped to `1..=1000` milliseconds.
    pub timeout_ms: u64,
    /// Cumulative path-storage/output budget, clamped to `1024..=1_048_576` bytes.
    pub max_bytes: usize,
}

impl Default for SearchPathsRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: SearchPathsScope::Tracked,
            refresh: false,
            directory: String::new(),
            recursive: true,
            include_hidden: false,
            max_results: 100,
            max_entries: 250_000,
            max_depth: 32,
            timeout_ms: 100,
            max_bytes: 262_144,
        }
    }
}

/// Truthful traversal outcome; empty complete results differ from unavailable search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPathsState {
    /// All permitted entries in the requested scope were examined.
    Complete,
    /// A budget, depth boundary, encoding limitation, or inaccessible entry stopped work.
    Incomplete,
    /// Invocation cancellation was observed.
    Cancelled,
    /// Context/authorization/platform/root was unavailable. No results are returned.
    Unavailable,
}

/// Kind of an insertion candidate. Symlinks and special files are never candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPathKind {
    /// Regular file (contents are not opened).
    File,
    /// Directory (consumer may navigate using this relative path).
    Directory,
}

/// Requested/effective path inventory scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPathsScope {
    /// Existing Git-index files, including staged additions.
    #[default]
    Tracked,
    /// Tracked and nonignored untracked files; nonrepo confined traversal.
    Workspace,
}

/// Half-open byte range in the original UTF-8 relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPathMatchRange {
    /// Inclusive byte offset.
    pub start: usize,
    /// Exclusive byte offset.
    pub end: usize,
}

/// Workspace-relative candidate, not a file attachment or a content reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPathEntry {
    /// Exact UTF-8 workspace-relative path; spaces and Unicode are preserved.
    pub path: String,
    /// File versus navigable directory.
    pub kind: SearchPathKind,
    /// Lower is better. Equal scores are ordered by exact relative path.
    pub score: usize,
    /// Matching original-path byte ranges, empty for an empty query.
    #[serde(default)]
    pub match_ranges: Vec<SearchPathMatchRange>,
}

/// Bounded path-only response; consumers must display non-complete states explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPathsResponse {
    /// Candidates in deterministic score/path order within the examined scope.
    pub entries: Vec<SearchPathEntry>,
    /// Whether results exhaust the requested authorized scope.
    pub state: SearchPathsState,
    /// Stable reason code, never an OS error containing sensitive paths.
    pub reason: Option<String>,
    /// Number of directory entries examined, including hidden/ignored entries.
    pub visited_entries: usize,
    /// Scope actually searched; nonrepo fallback requires explicit workspace scope.
    #[serde(default)]
    pub effective_scope: SearchPathsScope,
}

impl SearchPathsResponse {
    /// Construct an empty, explicitly non-successful search outcome.
    #[must_use]
    pub fn terminal(state: SearchPathsState, reason: &str) -> Self {
        Self {
            entries: Vec::new(),
            state,
            reason: Some(reason.into()),
            visited_entries: 0,
            effective_scope: SearchPathsScope::Tracked,
        }
    }
}
