//! Lazy, revision-pinned repository reads for reviews.
//!
//! Implements the lazy, revision-pinned operations declared in
//! `bcode_code_review_models::data`: source resolution and change tokens, the per-source file
//! index, per-file hunks, blob line ranges, paged reference, commit, and path search,
//! per-file stack layer attribution, and a repository state summary for review suggestions.
//! Every operation is bounded; none loads the whole review.
//!
//! State kept between calls is bounded and disposable: one `git cat-file --batch` process per
//! repository root (idle-evicted, restarted on failure), a byte-budgeted blob cache, a small
//! cache of immutable (commit-to-commit) file indexes, and resolved repository roots.

mod blob;
pub(crate) mod git;
mod hunks;
mod index;
mod rereview;
mod resolve;
mod search;
mod stack;

pub use blob::{BlobService, CatFileRegistry};

use bcode_code_review_models::{
    REVIEW_ERROR_CURSOR_STALE, REVIEW_ERROR_GIT_FAILED, REVIEW_ERROR_INVALID_REQUEST,
    REVIEW_ERROR_NOT_FOUND, REVIEW_ERROR_PATH_REJECTED, REVIEW_ERROR_TOO_LARGE,
    ResolveReviewSourcesRequest, ResolveReviewSourcesResponse, ReviewBlobLinesRequest,
    ReviewBlobLinesResponse, ReviewCommitsPageRequest, ReviewCommitsPageResponse,
    ReviewFileHunksRequest, ReviewFileHunksResponse, ReviewFilesSearchRequest,
    ReviewFilesSearchResponse, ReviewIndexManyRequest, ReviewIndexManyResponse, ReviewIndexOutcome,
    ReviewIndexRequest, ReviewIndexResponse, ReviewPatchIdsRequest, ReviewPatchIdsResponse,
    ReviewRefsSearchRequest, ReviewRefsSearchResponse, ReviewRepoStateRequest,
    ReviewRepoStateResponse, ReviewRevisionsCompareRequest, ReviewRevisionsCompareResponse,
    ReviewSourcesTokenRequest, ReviewSourcesTokenResponse, ReviewStackAttributionRequest,
    ReviewStackAttributionResponse,
};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

/// Threads one [`ReviewData::index_get_many`] call lists sources on.
const INDEX_WORKERS: usize = 8;

/// Maximum repository roots remembered by the root cache before it is cleared.
const MAX_CACHED_ROOTS: usize = 64;

/// Errors returned by [`ReviewData`] operations. Each maps to a stable service error code.
#[derive(Debug, Error)]
pub enum DataError {
    /// The request was malformed or used an unsupported combination.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// A git command failed.
    #[error("git command failed: {0}")]
    Git(String),
    /// The requested object, revision, or path does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A repository-relative path was rejected by path confinement.
    #[error("path rejected: {0}")]
    PathRejected(String),
    /// The requested content exceeds a service bound.
    #[error("too large: {0}")]
    TooLarge(String),
    /// A paged cursor no longer matches its listing.
    #[error("cursor is stale; restart from the first page")]
    CursorStale,
    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl DataError {
    /// Stable service error code for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => REVIEW_ERROR_INVALID_REQUEST,
            Self::Git(_) | Self::Io(_) => REVIEW_ERROR_GIT_FAILED,
            Self::NotFound(_) => REVIEW_ERROR_NOT_FOUND,
            Self::PathRejected(_) => REVIEW_ERROR_PATH_REJECTED,
            Self::TooLarge(_) => REVIEW_ERROR_TOO_LARGE,
            Self::CursorStale => REVIEW_ERROR_CURSOR_STALE,
        }
    }
}

/// Long-lived repository read state: resolved roots, `cat-file` readers, and caches of
/// immutable results (file indexes and merge bases between commits).
///
/// Every method takes `&self` and may be called from many threads at once: each cache has its own
/// short lock, held only to read or store an entry, never while git runs, so unrelated work (a
/// background index and the hunks on screen) proceeds in parallel. The number of git processes
/// running at once is bounded machine-wide (see `git`).
#[derive(Default)]
pub struct ReviewData {
    roots: Mutex<BTreeMap<PathBuf, PathBuf>>,
    blobs: Mutex<BlobService>,
    indexes: Mutex<index::IndexCache>,
    merge_bases: resolve::MergeBases,
}

impl ReviewData {
    /// Stop `cat-file` processes that have been idle past their limit. There is no background
    /// timer, so long-lived owners call this between requests.
    pub fn evict_idle(&self) {
        lock(&self.blobs).registry_mut().evict_idle();
    }

    /// Resolve included workspace sources to revision-pinned sources.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] when the repository path is invalid.
    /// * [`DataError::Git`] / [`DataError::Io`] when git cannot run. Per-source failures
    ///   are reported as diagnostics, not errors.
    pub fn resolve_sources(
        &self,
        request: &ResolveReviewSourcesRequest,
    ) -> Result<ResolveReviewSourcesResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        resolve::resolve_sources(
            &root,
            &request.sources,
            &request.provider_resolutions,
            &self.merge_bases,
        )
    }

    /// Attribute one file of a resolved stack's whole-stack diff to the layers that introduced
    /// (new side) or removed (old side) each line.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] for invalid object ids, an empty stack, or more than
    ///   the maximum number of layers.
    /// * [`DataError::PathRejected`] for unconfined paths.
    /// * [`DataError::TooLarge`] when the file exceeds the attribution line bound.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn stack_attribution(
        &self,
        request: &ReviewStackAttributionRequest,
    ) -> Result<ReviewStackAttributionResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        stack::attribution(&root, request)
    }

    /// Summarize repository state for review suggestions (branch, detected base, commits ahead,
    /// unstaged and staged change counts).
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] when the repository path is invalid.
    /// * [`DataError::Git`] / [`DataError::Io`] when git cannot run.
    pub fn repo_state(
        &self,
        request: &ReviewRepoStateRequest,
    ) -> Result<ReviewRepoStateResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        resolve::repo_state(&root)
    }

    /// Compare a previously reviewed commit with the current one (re-review).
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] for invalid object ids or repository paths.
    /// * [`DataError::NotFound`] when the new commit does not exist.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn revisions_compare(
        &self,
        request: &ReviewRevisionsCompareRequest,
    ) -> Result<ReviewRevisionsCompareResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        rereview::compare(&root, request)
    }

    /// Patch ids of several files of one revision pair (re-review).
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] for too many paths or an unsupported revision pair.
    /// * [`DataError::PathRejected`] for unconfined paths.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn patch_ids_get(
        &self,
        request: &ReviewPatchIdsRequest,
    ) -> Result<ReviewPatchIdsResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        rereview::patch_ids(&root, request)
    }

    /// Compute the cheap change token for working-tree and index sources.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] when the repository path is invalid.
    /// * [`DataError::Git`] / [`DataError::Io`] when git or the index cannot be read.
    pub fn sources_token(
        &self,
        request: &ReviewSourcesTokenRequest,
    ) -> Result<ReviewSourcesTokenResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        resolve::sources_token(&root, request.include_working_tree)
    }

    /// Return one page of a resolved source's file index.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] for invalid revisions, paths, or cursors.
    /// * [`DataError::CursorStale`] when the listing changed since the cursor was issued.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn index_get(
        &self,
        request: &ReviewIndexRequest,
    ) -> Result<ReviewIndexResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        index::index_page(&self.indexes, &root, request)
    }

    /// Return the first page of many resolved sources' file indexes, listed in parallel (git
    /// processes stay within the machine-wide bound). A source that fails reports its error in
    /// its outcome; the others are unaffected.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] when the repository path is invalid.
    pub fn index_get_many(
        &self,
        request: &ReviewIndexManyRequest,
    ) -> Result<ReviewIndexManyResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        let workers = request.sources.len().clamp(1, INDEX_WORKERS);
        let chunk = request.sources.len().div_ceil(workers).max(1);
        let indexes = std::thread::scope(|scope| {
            // Every worker starts before any is joined, so they run at once.
            let mut handles = Vec::new();
            for sources in request.sources.chunks(chunk) {
                let root = &root;
                handles.push(scope.spawn(move || {
                    sources
                        .iter()
                        .map(|source| {
                            let page = index::index_page(
                                &self.indexes,
                                root,
                                &ReviewIndexRequest {
                                    repo_path: request.repo_path.clone(),
                                    source: source.clone(),
                                    cursor: None,
                                    limit: request.limit,
                                },
                            );
                            let (index, error) = match page {
                                Ok(index) => (Some(index), None),
                                Err(error) => (None, Some(error)),
                            };
                            ReviewIndexOutcome {
                                source_id: source.source_id.clone(),
                                index,
                                error_code: error.as_ref().map(|error| error.code().to_owned()),
                                error_message: error.map(|error| error.to_string()),
                            }
                        })
                        .collect::<Vec<_>>()
                }));
            }
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        Ok(ReviewIndexManyResponse { indexes })
    }

    /// Return the hunks of one file of one resolved source.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] for browse sources or invalid revisions.
    /// * [`DataError::PathRejected`] for unconfined paths.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn file_hunks_get(
        &self,
        request: &ReviewFileHunksRequest,
    ) -> Result<ReviewFileHunksResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        hunks::file_hunks(&root, request)
    }

    /// Return a line range of one blob.
    ///
    /// # Errors
    ///
    /// * [`DataError::NotFound`] when the object or path does not exist.
    /// * [`DataError::PathRejected`] for unconfined paths.
    /// * [`DataError::TooLarge`] for blobs above the size bound.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails twice in a row.
    pub fn blob_lines_get(
        &self,
        request: &ReviewBlobLinesRequest,
    ) -> Result<ReviewBlobLinesResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        lock(&self.blobs).lines(&root, request)
    }

    /// Return one page of references matching a query.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] / [`DataError::CursorStale`] for bad cursors.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn refs_search(
        &self,
        request: &ReviewRefsSearchRequest,
    ) -> Result<ReviewRefsSearchResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        search::refs_search(&root, request)
    }

    /// Return one page of the commit log.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] for invalid revisions or cursors.
    /// * [`DataError::NotFound`] when the start revision does not exist.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn commits_page(
        &self,
        request: &ReviewCommitsPageRequest,
    ) -> Result<ReviewCommitsPageResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        search::commits_page(&root, request)
    }

    /// Return one page of repository paths matching a query.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] / [`DataError::CursorStale`] for bad cursors.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn files_search(
        &self,
        request: &ReviewFilesSearchRequest,
    ) -> Result<ReviewFilesSearchResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        search::files_search(&root, request)
    }

    /// Blob reader state, for inspection in tests.
    #[cfg(test)]
    pub(crate) fn blobs_mut(&self) -> std::sync::MutexGuard<'_, BlobService> {
        lock(&self.blobs)
    }

    fn repo_root(&self, repo_path: &Path) -> Result<PathBuf, DataError> {
        if let Some(root) = lock(&self.roots).get(repo_path) {
            return Ok(root.clone());
        }
        if !repo_path.is_absolute() {
            return Err(DataError::InvalidRequest(format!(
                "repo_path must be absolute: {}",
                repo_path.display()
            )));
        }
        if !repo_path.is_dir() {
            return Err(DataError::InvalidRequest(format!(
                "repo_path is not a directory: {}",
                repo_path.display()
            )));
        }
        let top = git::output(repo_path, &["rev-parse", "--show-toplevel"])?;
        let root = std::fs::canonicalize(String::from_utf8_lossy(&top).trim())?;
        {
            let mut roots = lock(&self.roots);
            if roots.len() >= MAX_CACHED_ROOTS {
                roots.clear();
            }
            roots.insert(repo_path.to_path_buf(), root.clone());
        }
        Ok(root)
    }
}

/// Lock `mutex`, recovering the guard when a holder panicked: caches hold only derived data, so a
/// panic mid-update loses at most an entry.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Short hexadecimal fingerprint of `parts`, used to bind cursors to their listing.
fn fingerprint(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(part);
    }
    hex::encode(&hasher.finalize()[..8])
}

/// Encode an offset cursor bound to a listing fingerprint.
fn encode_cursor(offset: usize, fingerprint: &str) -> String {
    format!("{offset}.{fingerprint}")
}

/// Decode an offset cursor and check it against the current listing fingerprint.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] when the cursor is malformed.
/// * [`DataError::CursorStale`] when the fingerprint differs.
fn decode_cursor(cursor: Option<&str>, fingerprint: &str) -> Result<usize, DataError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (offset, expected) = cursor
        .split_once('.')
        .ok_or_else(|| DataError::InvalidRequest(format!("malformed cursor: {cursor:?}")))?;
    let offset = offset
        .parse::<usize>()
        .map_err(|_| DataError::InvalidRequest(format!("malformed cursor: {cursor:?}")))?;
    if expected != fingerprint {
        return Err(DataError::CursorStale);
    }
    Ok(offset)
}

/// Clamp a requested page size to `1..=max`, defaulting to `default`.
fn page_size(limit: Option<u32>, default: u32, max: u32) -> usize {
    usize::try_from(limit.unwrap_or(default).clamp(1, max)).unwrap_or(usize::MAX)
}

/// Return the page `[offset, offset + size)` of `items` and the next cursor, if any.
fn page<T: Clone>(
    items: &[T],
    offset: usize,
    size: usize,
    fingerprint: &str,
) -> Result<(Vec<T>, Option<String>), DataError> {
    if offset > items.len() {
        return Err(DataError::CursorStale);
    }
    let end = offset.saturating_add(size).min(items.len());
    let next = (end < items.len()).then(|| encode_cursor(end, fingerprint));
    Ok((items[offset..end].to_vec(), next))
}

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod tests;
