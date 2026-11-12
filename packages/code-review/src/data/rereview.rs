//! Re-review primitives: how a previously reviewed commit relates to the current one
//! (`review.revisions.compare`), and the patch ids of several files of one revision pair
//! (`review.patch_ids.get`), so files whose patch survived a rebase can be recognized.
//!
//! Both are bounded: the comparison is one `cat-file -e` per commit, one `merge-base
//! --is-ancestor`, and one capped `rev-list --count`; patch ids are one `git diff` limited to at
//! most [`REVIEW_PATCH_IDS_MAX_PATHS`] paths.

use super::index::{DIFF_COMMON, revision_args};
use super::{DataError, git, hunks};
use bcode_code_review_models::{
    REVIEW_PATCH_IDS_MAX_PATHS, REVIEW_REVISIONS_MAX_AHEAD, ReviewPatchIdsRequest,
    ReviewPatchIdsResponse, ReviewRevisionsCompareRequest, ReviewRevisionsCompareResponse,
};
use std::path::Path;

/// Compare the commit reviewed last time with the current one.
pub fn compare(
    root: &Path,
    request: &ReviewRevisionsCompareRequest,
) -> Result<ReviewRevisionsCompareResponse, DataError> {
    git::validate_oid(&request.old_oid)?;
    git::validate_oid(&request.new_oid)?;
    let exists = |oid: &str| -> Result<bool, DataError> {
        let spec = format!("{oid}^{{commit}}");
        Ok(git::optional_output(root, &["cat-file", "-e", &spec])?.is_some())
    };
    if !exists(&request.new_oid)? {
        return Err(DataError::NotFound(format!(
            "commit {} does not exist",
            request.new_oid
        )));
    }
    if !exists(&request.old_oid)? {
        return Ok(ReviewRevisionsCompareResponse {
            old_exists: false,
            fast_forward: false,
            commits_ahead: None,
        });
    }
    let fast_forward = git::optional_output(
        root,
        &[
            "merge-base",
            "--is-ancestor",
            &request.old_oid,
            &request.new_oid,
        ],
    )?
    .is_some();
    let range = format!("{}..{}", request.old_oid, request.new_oid);
    let max = format!("--max-count={REVIEW_REVISIONS_MAX_AHEAD}");
    let count = git::output(root, &["rev-list", "--count", &max, &range])?;
    let commits_ahead = String::from_utf8_lossy(&count).trim().parse::<u32>().ok();
    Ok(ReviewRevisionsCompareResponse {
        old_exists: true,
        fast_forward,
        commits_ahead,
    })
}

/// Patch ids of the requested paths between `base` and `head`.
pub fn patch_ids(
    root: &Path,
    request: &ReviewPatchIdsRequest,
) -> Result<ReviewPatchIdsResponse, DataError> {
    if request.paths.len() > REVIEW_PATCH_IDS_MAX_PATHS {
        return Err(DataError::InvalidRequest(format!(
            "at most {REVIEW_PATCH_IDS_MAX_PATHS} paths per request"
        )));
    }
    let mut response = ReviewPatchIdsResponse::default();
    if request.paths.is_empty() {
        return Ok(response);
    }
    for path in &request.paths {
        git::validate_relative_path(path)?;
    }
    let mut args: Vec<String> = ["diff", "--src-prefix=a/", "--dst-prefix=b/", "--unified=0"]
        .into_iter()
        .chain(DIFF_COMMON)
        .map(str::to_string)
        .collect();
    args.extend(revision_args(root, &request.base, &request.head)?);
    args.push("--".to_string());
    args.extend(request.paths.iter().cloned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git::output(root, &args)?;
    let files = bcode_git::diff::parse_unified_diff(&String::from_utf8_lossy(&output))
        .map_err(|error| DataError::Git(format!("failed to parse diff: {error}")))?;
    let requested: std::collections::BTreeSet<&str> =
        request.paths.iter().map(String::as_str).collect();
    let mut merged: std::collections::BTreeMap<String, bcode_git::models::DiffFile> =
        std::collections::BTreeMap::new();
    for file in files {
        // A type change prints as a deletion and an addition of one path; merge them like
        // `review.file.hunks.get` does so both report the same identity.
        let key = [file.new_path.as_deref(), file.old_path.as_deref()]
            .into_iter()
            .flatten()
            .find(|path| requested.contains(path))
            .map(str::to_string);
        let Some(key) = key else {
            continue;
        };
        match merged.get_mut(&key) {
            Some(existing) => {
                existing.is_binary |= file.is_binary;
                if file.new_mode.is_some() {
                    existing.new_mode = file.new_mode;
                }
                existing.hunks.extend(file.hunks);
            }
            None => {
                merged.insert(key, file);
            }
        }
    }
    for (path, file) in merged {
        if file.is_binary {
            continue;
        }
        let id = hunks::patch_id(Some(&file), &file.hunks);
        // A renamed file answers for both of its names when both were asked for.
        for name in [file.old_path.as_deref(), file.new_path.as_deref()]
            .into_iter()
            .flatten()
            .filter(|name| requested.contains(name))
        {
            response.patch_ids.insert(name.to_string(), id.clone());
        }
        response.patch_ids.entry(path).or_insert(id);
    }
    Ok(response)
}
