//! Per-file hunks (`review.file.hunks.get`) and patch identity.

use super::index::{DIFF_COMMON, revision_args};
use super::{DataError, git};
use bcode_code_review_models::{
    REVIEW_HUNKS_DEFAULT_CONTEXT, REVIEW_HUNKS_DEFAULT_MAX_LINES, REVIEW_HUNKS_MAX_CONTEXT,
    REVIEW_HUNKS_MAX_LINES, ResolvedSourceMode, ReviewFileHunksRequest, ReviewFileHunksResponse,
    ReviewHunk, ReviewHunksTruncation, ReviewLine, ReviewLineKind,
};
use bcode_git::models::{DiffFile, DiffHunk, DiffLineKind};
use sha2::{Digest as _, Sha256};
use std::path::Path;

/// Load and parse the diff of one file.
pub fn file_hunks(
    root: &Path,
    request: &ReviewFileHunksRequest,
) -> Result<ReviewFileHunksResponse, DataError> {
    if request.source.mode == ResolvedSourceMode::Browse {
        return Err(DataError::InvalidRequest(
            "browse sources have no hunks; read content with review.blob.lines.get".to_string(),
        ));
    }
    git::validate_relative_path(&request.path)?;
    if let Some(old_path) = &request.old_path {
        git::validate_relative_path(old_path)?;
    }
    let context = request
        .context_lines
        .unwrap_or(REVIEW_HUNKS_DEFAULT_CONTEXT)
        .min(REVIEW_HUNKS_MAX_CONTEXT);
    let max_lines = request
        .max_lines
        .unwrap_or(REVIEW_HUNKS_DEFAULT_MAX_LINES)
        .clamp(1, REVIEW_HUNKS_MAX_LINES);

    let mut args: Vec<String> = ["diff", "--src-prefix=a/", "--dst-prefix=b/"]
        .into_iter()
        .chain(DIFF_COMMON)
        .map(str::to_string)
        .collect();
    args.push(format!("--unified={context}"));
    if request.ignore_whitespace {
        args.push("--ignore-all-space".to_string());
    }
    args.extend(revision_args(
        root,
        &request.source.base,
        &request.source.head,
    )?);
    args.push("--".to_string());
    if let Some(old_path) = request
        .old_path
        .as_ref()
        .filter(|old| **old != request.path)
    {
        args.push(old_path.clone());
    }
    args.push(request.path.clone());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git::output(root, &args)?;
    let files = bcode_git::diff::parse_unified_diff(&String::from_utf8_lossy(&output))
        .map_err(|error| DataError::Git(format!("failed to parse diff: {error}")))?;
    let file = select_file(files, &request.path);

    let mut response = ReviewFileHunksResponse {
        source_id: request.source.source_id.clone(),
        path: request.path.clone(),
        hunks: Vec::new(),
        binary: false,
        truncated: None,
        patch_id: None,
    };
    let Some(file) = file else {
        // No textual difference (for example only whitespace changed under `-w`).
        if !request.ignore_whitespace {
            response.patch_id = Some(patch_id(None, &[]));
        }
        return Ok(response);
    };
    if file.is_binary {
        response.binary = true;
        return Ok(response);
    }
    if !request.ignore_whitespace {
        response.patch_id = Some(patch_id(Some(&file), &file.hunks));
    }
    let (hunks, truncated) = truncate(&file.hunks, max_lines);
    response.hunks = hunks;
    response.truncated = truncated;
    Ok(response)
}

/// The diff entry for `path`. A type change (for example a file replaced by a symbolic link)
/// is printed by git as a deletion followed by an addition of the same path; both are merged
/// into one entry so the response covers the whole change. When no entry names `path` (for
/// example a path that could not be decoded) but the pathspec-limited diff has exactly one
/// entry, that entry is used.
fn select_file(files: Vec<DiffFile>, path: &str) -> Option<DiffFile> {
    let names = |file: &DiffFile| {
        file.new_path.as_deref() == Some(path) || file.old_path.as_deref() == Some(path)
    };
    let (mut matching, rest): (Vec<DiffFile>, Vec<DiffFile>) = files.into_iter().partition(names);
    if matching.is_empty() {
        return match <[DiffFile; 1]>::try_from(rest) {
            Ok([only]) => Some(only),
            Err(_) => None,
        };
    }
    let mut merged = matching.remove(0);
    for part in matching {
        merged.is_binary |= part.is_binary;
        if part.new_mode.is_some() {
            merged.new_mode = part.new_mode;
        }
        if part.new_path.is_some() {
            merged.new_path = part.new_path;
        }
        merged.hunks.extend(part.hunks);
    }
    Some(merged)
}

/// SHA-256 over the changed lines in order (marker plus raw text, no line numbers, no context)
/// and any mode change. Rebasing a patch onto a different base keeps this identity as long as
/// its changed lines are the same.
pub(super) fn patch_id(file: Option<&DiffFile>, hunks: &[DiffHunk]) -> String {
    let mut hasher = Sha256::new();
    if let Some(file) = file
        && let (Some(old), Some(new)) = (&file.old_mode, &file.new_mode)
        && old != new
    {
        hasher.update(format!("mode {old} {new}\n").as_bytes());
    }
    for line in hunks.iter().flat_map(|hunk| &hunk.lines) {
        let marker: &[u8] = match line.kind {
            DiffLineKind::Added => b"+",
            DiffLineKind::Removed => b"-",
            DiffLineKind::Context => continue,
        };
        hasher.update(marker);
        hasher.update(line.content.as_bytes());
        hasher.update(if line.no_newline {
            &b"\n\\\n"[..]
        } else {
            &b"\n"[..]
        });
    }
    hex::encode(hasher.finalize())
}

fn review_line(line: &bcode_git::models::DiffLine) -> ReviewLine {
    ReviewLine {
        kind: match line.kind {
            DiffLineKind::Context => ReviewLineKind::Context,
            DiffLineKind::Added => ReviewLineKind::Added,
            DiffLineKind::Removed => ReviewLineKind::Removed,
        },
        old_line: line.old_line,
        new_line: line.new_line,
        content: line
            .content
            .strip_suffix('\r')
            .unwrap_or(&line.content)
            .to_string(),
    }
}

/// Convert hunks, cutting at `max_lines` diff lines. A cut hunk keeps its start lines and gets
/// counts that describe exactly the lines it still contains.
fn truncate(
    hunks: &[DiffHunk],
    max_lines: u32,
) -> (Vec<ReviewHunk>, Option<ReviewHunksTruncation>) {
    let total_lines: usize = hunks.iter().map(|hunk| hunk.lines.len()).sum();
    let budget = usize::try_from(max_lines).unwrap_or(usize::MAX);
    let mut remaining = budget;
    let mut out = Vec::new();
    for hunk in hunks {
        if remaining == 0 {
            break;
        }
        let take = hunk.lines.len().min(remaining);
        remaining -= take;
        let lines: Vec<ReviewLine> = hunk.lines[..take].iter().map(review_line).collect();
        let cut = take < hunk.lines.len();
        let count = |side: fn(&ReviewLine) -> bool| {
            u32::try_from(lines.iter().filter(|line| side(line)).count()).unwrap_or(u32::MAX)
        };
        out.push(ReviewHunk {
            old_start: hunk.old_start,
            old_count: if cut {
                count(|line| line.old_line.is_some())
            } else {
                hunk.old_count
            },
            new_start: hunk.new_start,
            new_count: if cut {
                count(|line| line.new_line.is_some())
            } else {
                hunk.new_count
            },
            heading: hunk.heading.clone(),
            lines,
        });
    }
    if total_lines <= budget {
        return (out, None);
    }
    let to_u32 = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    let truncation = ReviewHunksTruncation {
        shown_lines: to_u32(budget),
        total_lines: to_u32(total_lines),
        shown_hunks: to_u32(out.len()),
        total_hunks: to_u32(hunks.len()),
    };
    (out, Some(truncation))
}
