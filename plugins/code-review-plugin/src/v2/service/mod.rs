//! Code Review V2 data layer service (daemon side).
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
    OP_REVIEW_BLOB_LINES_GET, OP_REVIEW_COMMITS_PAGE, OP_REVIEW_FILE_HUNKS_GET,
    OP_REVIEW_FILES_SEARCH, OP_REVIEW_INDEX_GET, OP_REVIEW_PATCH_IDS_GET, OP_REVIEW_REFS_SEARCH,
    OP_REVIEW_REPO_STATE, OP_REVIEW_REVISIONS_COMPARE, OP_REVIEW_SOURCES_RESOLVE,
    OP_REVIEW_SOURCES_TOKEN, OP_REVIEW_STACK_ATTRIBUTION, REVIEW_ERROR_CURSOR_STALE,
    REVIEW_ERROR_GIT_FAILED, REVIEW_ERROR_INVALID_REQUEST, REVIEW_ERROR_NOT_FOUND,
    REVIEW_ERROR_PATH_REJECTED, REVIEW_ERROR_TOO_LARGE, ResolveReviewSourcesRequest,
    ResolveReviewSourcesResponse, ReviewBlobLinesRequest, ReviewBlobLinesResponse,
    ReviewCommitsPageRequest, ReviewCommitsPageResponse, ReviewFileHunksRequest,
    ReviewFileHunksResponse, ReviewFilesSearchRequest, ReviewFilesSearchResponse,
    ReviewIndexRequest, ReviewIndexResponse, ReviewPatchIdsRequest, ReviewPatchIdsResponse,
    ReviewRefsSearchRequest, ReviewRefsSearchResponse, ReviewRepoStateRequest,
    ReviewRepoStateResponse, ReviewRevisionsCompareRequest, ReviewRevisionsCompareResponse,
    ReviewSourcesTokenRequest, ReviewSourcesTokenResponse, ReviewStackAttributionRequest,
    ReviewStackAttributionResponse,
};
use bcode_plugin_sdk::prelude::{ServiceRequest, ServiceResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Maximum repository roots remembered by the root cache before it is cleared.
const MAX_CACHED_ROOTS: usize = 64;

/// Errors returned by V2 data layer operations. Each maps to a stable service error code.
#[derive(Debug, Error)]
pub enum ServiceError {
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

impl ServiceError {
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

/// Long-lived V2 data layer state owned by the plugin instance.
#[derive(Default)]
pub struct V2Service {
    roots: BTreeMap<PathBuf, PathBuf>,
    blobs: BlobService,
    indexes: index::IndexCache,
}

impl V2Service {
    /// Handle a V2 data layer operation, or return `None` when `request` is not one.
    pub fn invoke(&mut self, request: &ServiceRequest) -> Option<ServiceResponse> {
        // There is no background timer; idle `cat-file` processes are stopped on the next call.
        self.blobs.registry_mut().evict_idle();
        let response = match request.operation.as_str() {
            OP_REVIEW_SOURCES_RESOLVE => handle(request, |req| self.resolve_sources(&req)),
            OP_REVIEW_SOURCES_TOKEN => handle(request, |req| self.sources_token(&req)),
            OP_REVIEW_INDEX_GET => handle(request, |req| self.index_get(&req)),
            OP_REVIEW_FILE_HUNKS_GET => handle(request, |req| self.file_hunks_get(&req)),
            OP_REVIEW_BLOB_LINES_GET => handle(request, |req| self.blob_lines_get(&req)),
            OP_REVIEW_REFS_SEARCH => handle(request, |req| self.refs_search(&req)),
            OP_REVIEW_COMMITS_PAGE => handle(request, |req| self.commits_page(&req)),
            OP_REVIEW_FILES_SEARCH => handle(request, |req| self.files_search(&req)),
            OP_REVIEW_STACK_ATTRIBUTION => handle(request, |req| self.stack_attribution(&req)),
            OP_REVIEW_REPO_STATE => handle(request, |req| self.repo_state(&req)),
            OP_REVIEW_REVISIONS_COMPARE => handle(request, |req| self.revisions_compare(&req)),
            OP_REVIEW_PATCH_IDS_GET => handle(request, |req| self.patch_ids_get(&req)),
            _ => return None,
        };
        Some(response)
    }

    /// Resolve included workspace sources to revision-pinned sources.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] when the repository path is invalid.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git cannot run. Per-source failures
    ///   are reported as diagnostics, not errors.
    pub fn resolve_sources(
        &mut self,
        request: &ResolveReviewSourcesRequest,
    ) -> Result<ResolveReviewSourcesResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        resolve::resolve_sources(&root, &request.sources, &request.provider_resolutions)
    }

    /// Attribute one file of a resolved stack's whole-stack diff to the layers that introduced
    /// (new side) or removed (old side) each line.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] for invalid object ids, an empty stack, or more than
    ///   the maximum number of layers.
    /// * [`ServiceError::PathRejected`] for unconfined paths.
    /// * [`ServiceError::TooLarge`] when the file exceeds the attribution line bound.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn stack_attribution(
        &mut self,
        request: &ReviewStackAttributionRequest,
    ) -> Result<ReviewStackAttributionResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        stack::attribution(&root, request)
    }

    /// Summarize repository state for review suggestions (branch, detected base, commits ahead,
    /// unstaged and staged change counts).
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] when the repository path is invalid.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git cannot run.
    pub fn repo_state(
        &mut self,
        request: &ReviewRepoStateRequest,
    ) -> Result<ReviewRepoStateResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        resolve::repo_state(&root)
    }

    /// Compare a previously reviewed commit with the current one (re-review, spec 7.4).
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] for invalid object ids or repository paths.
    /// * [`ServiceError::NotFound`] when the new commit does not exist.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn revisions_compare(
        &mut self,
        request: &ReviewRevisionsCompareRequest,
    ) -> Result<ReviewRevisionsCompareResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        rereview::compare(&root, request)
    }

    /// Patch ids of several files of one revision pair (re-review, spec 7.4).
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] for too many paths or an unsupported revision pair.
    /// * [`ServiceError::PathRejected`] for unconfined paths.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn patch_ids_get(
        &mut self,
        request: &ReviewPatchIdsRequest,
    ) -> Result<ReviewPatchIdsResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        rereview::patch_ids(&root, request)
    }

    /// Compute the cheap change token for working-tree and index sources.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] when the repository path is invalid.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git or the index cannot be read.
    pub fn sources_token(
        &mut self,
        request: &ReviewSourcesTokenRequest,
    ) -> Result<ReviewSourcesTokenResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        resolve::sources_token(&root, request.include_working_tree)
    }

    /// Return one page of a resolved source's file index.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] for invalid revisions, paths, or cursors.
    /// * [`ServiceError::CursorStale`] when the listing changed since the cursor was issued.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn index_get(
        &mut self,
        request: &ReviewIndexRequest,
    ) -> Result<ReviewIndexResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        index::index_page(&mut self.indexes, &root, request)
    }

    /// Return the hunks of one file of one resolved source.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] for browse sources or invalid revisions.
    /// * [`ServiceError::PathRejected`] for unconfined paths.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn file_hunks_get(
        &mut self,
        request: &ReviewFileHunksRequest,
    ) -> Result<ReviewFileHunksResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        hunks::file_hunks(&root, request)
    }

    /// Return a line range of one blob.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::NotFound`] when the object or path does not exist.
    /// * [`ServiceError::PathRejected`] for unconfined paths.
    /// * [`ServiceError::TooLarge`] for blobs above the size bound.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails twice in a row.
    pub fn blob_lines_get(
        &mut self,
        request: &ReviewBlobLinesRequest,
    ) -> Result<ReviewBlobLinesResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        self.blobs.lines(&root, request)
    }

    /// Return one page of references matching a query.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] / [`ServiceError::CursorStale`] for bad cursors.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn refs_search(
        &mut self,
        request: &ReviewRefsSearchRequest,
    ) -> Result<ReviewRefsSearchResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        search::refs_search(&root, request)
    }

    /// Return one page of the commit log.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] for invalid revisions or cursors.
    /// * [`ServiceError::NotFound`] when the start revision does not exist.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn commits_page(
        &mut self,
        request: &ReviewCommitsPageRequest,
    ) -> Result<ReviewCommitsPageResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        search::commits_page(&root, request)
    }

    /// Return one page of repository paths matching a query.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::InvalidRequest`] / [`ServiceError::CursorStale`] for bad cursors.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when git fails.
    pub fn files_search(
        &mut self,
        request: &ReviewFilesSearchRequest,
    ) -> Result<ReviewFilesSearchResponse, ServiceError> {
        let root = self.repo_root(&request.repo_path)?;
        search::files_search(&root, request)
    }

    /// Blob reader state, for inspection in tests.
    #[cfg(test)]
    pub(crate) const fn blobs_mut(&mut self) -> &mut BlobService {
        &mut self.blobs
    }

    fn repo_root(&mut self, repo_path: &Path) -> Result<PathBuf, ServiceError> {
        if let Some(root) = self.roots.get(repo_path) {
            return Ok(root.clone());
        }
        if !repo_path.is_absolute() {
            return Err(ServiceError::InvalidRequest(format!(
                "repo_path must be absolute: {}",
                repo_path.display()
            )));
        }
        if !repo_path.is_dir() {
            return Err(ServiceError::InvalidRequest(format!(
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

fn handle<Req, Resp>(
    request: &ServiceRequest,
    operation: impl FnOnce(Req) -> Result<Resp, ServiceError>,
) -> ServiceResponse
where
    Req: DeserializeOwned,
    Resp: Serialize,
{
    let payload = match request.payload_json::<Req>() {
        Ok(payload) => payload,
        Err(error) => {
            return ServiceResponse::error(REVIEW_ERROR_INVALID_REQUEST, error.to_string());
        }
    };
    match operation(payload) {
        Ok(response) => crate::json_response(&response),
        Err(error) => ServiceResponse::error(error.code(), error.to_string()),
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
/// * [`ServiceError::InvalidRequest`] when the cursor is malformed.
/// * [`ServiceError::CursorStale`] when the fingerprint differs.
fn decode_cursor(cursor: Option<&str>, fingerprint: &str) -> Result<usize, ServiceError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (offset, expected) = cursor
        .split_once('.')
        .ok_or_else(|| ServiceError::InvalidRequest(format!("malformed cursor: {cursor:?}")))?;
    let offset = offset
        .parse::<usize>()
        .map_err(|_| ServiceError::InvalidRequest(format!("malformed cursor: {cursor:?}")))?;
    if expected != fingerprint {
        return Err(ServiceError::CursorStale);
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
) -> Result<(Vec<T>, Option<String>), ServiceError> {
    if offset > items.len() {
        return Err(ServiceError::CursorStale);
    }
    let end = offset.saturating_add(size).min(items.len());
    let next = (end < items.len()).then(|| encode_cursor(end, fingerprint));
    Ok((items[offset..end].to_vec(), next))
}

#[cfg(test)]
mod tests;
