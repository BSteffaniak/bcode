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
mod git;
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
    ReviewFilesSearchResponse, ReviewIndexRequest, ReviewIndexResponse, ReviewPatchIdsRequest,
    ReviewPatchIdsResponse, ReviewRefsSearchRequest, ReviewRefsSearchResponse,
    ReviewRepoStateRequest, ReviewRepoStateResponse, ReviewRevisionsCompareRequest,
    ReviewRevisionsCompareResponse, ReviewSourcesTokenRequest, ReviewSourcesTokenResponse,
    ReviewStackAttributionRequest, ReviewStackAttributionResponse,
};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

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

/// Long-lived repository read state: resolved roots, `cat-file` readers, and index caches.
///
/// Every method takes `&mut self` because it may update the caches; share one instance behind a
/// lock when several tasks read concurrently.
#[derive(Default)]
pub struct ReviewData {
    roots: BTreeMap<PathBuf, PathBuf>,
    blobs: BlobService,
    indexes: index::IndexCache,
}

impl ReviewData {
    /// Stop `cat-file` processes that have been idle past their limit. There is no background
    /// timer, so long-lived owners call this between requests.
    pub fn evict_idle(&mut self) {
        self.blobs.registry_mut().evict_idle();
    }

    /// Resolve included workspace sources to revision-pinned sources.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] when the repository path is invalid.
    /// * [`DataError::Git`] / [`DataError::Io`] when git cannot run. Per-source failures
    ///   are reported as diagnostics, not errors.
    pub fn resolve_sources(
        &mut self,
        request: &ResolveReviewSourcesRequest,
    ) -> Result<ResolveReviewSourcesResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        resolve::resolve_sources(&root, &request.sources, &request.provider_resolutions)
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
        &mut self,
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
        &mut self,
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
        &mut self,
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
        &mut self,
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
        &mut self,
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
        &mut self,
        request: &ReviewIndexRequest,
    ) -> Result<ReviewIndexResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        index::index_page(&mut self.indexes, &root, request)
    }

    /// Return the hunks of one file of one resolved source.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] for browse sources or invalid revisions.
    /// * [`DataError::PathRejected`] for unconfined paths.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn file_hunks_get(
        &mut self,
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
        &mut self,
        request: &ReviewBlobLinesRequest,
    ) -> Result<ReviewBlobLinesResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        self.blobs.lines(&root, request)
    }

    /// Return one page of references matching a query.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidRequest`] / [`DataError::CursorStale`] for bad cursors.
    /// * [`DataError::Git`] / [`DataError::Io`] when git fails.
    pub fn refs_search(
        &mut self,
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
        &mut self,
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
        &mut self,
        request: &ReviewFilesSearchRequest,
    ) -> Result<ReviewFilesSearchResponse, DataError> {
        let root = self.repo_root(&request.repo_path)?;
        search::files_search(&root, request)
    }

    /// Blob reader state, for inspection in tests.
    #[cfg(test)]
    pub(crate) const fn blobs_mut(&mut self) -> &mut BlobService {
        &mut self.blobs
    }

    fn repo_root(&mut self, repo_path: &Path) -> Result<PathBuf, DataError> {
        if let Some(root) = self.roots.get(repo_path) {
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
        if self.roots.len() >= MAX_CACHED_ROOTS {
            self.roots.clear();
        }
        self.roots.insert(repo_path.to_path_buf(), root.clone());
        Ok(root)
    }
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
