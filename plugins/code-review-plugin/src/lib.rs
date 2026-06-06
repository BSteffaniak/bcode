#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled local Git code review plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

/// Code review plugin service interface.
pub const CODE_REVIEW_SERVICE_INTERFACE_ID: &str = "bcode.code_review/v1";

/// Operation that creates an ephemeral local review from a Git target.
pub const OP_CREATE_REVIEW: &str = "create_review";

/// Bundled local code review plugin.
#[derive(Default)]
pub struct CodeReviewPlugin;

impl RustPlugin for CodeReviewPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != CODE_REVIEW_SERVICE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported code review plugin service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_CREATE_REVIEW => create_review(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported code review service operation",
            ),
        }
    }
}

/// Request payload for `create_review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReviewRequest {
    /// Repository path where Git commands should run.
    pub repo_path: PathBuf,
    /// Local Git target to review.
    pub target: ReviewTarget,
}

/// Supported local Git review target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    /// Review unstaged working-tree changes.
    WorkingTreeUnstaged,
    /// Review staged index changes.
    IndexStaged,
    /// Review both staged and unstaged changes.
    WorkingTreeAndIndex,
    /// Review the last commit.
    LastCommit,
    /// Review an explicit commit range.
    CommitRange {
        /// Base revision.
        base: String,
        /// Head revision.
        head: String,
        /// Whether to use merge-base `...` semantics.
        #[serde(default)]
        merge_base: bool,
    },
    /// Review a branch comparison.
    BranchCompare {
        /// Base branch.
        base_branch: String,
        /// Head branch.
        head_branch: String,
        /// Whether to use merge-base `...` semantics.
        #[serde(default = "default_true")]
        merge_base: bool,
    },
}

/// Structured review response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    /// Human-readable target label.
    pub title: String,
    /// Repository root resolved by Git.
    pub repo_root: PathBuf,
    /// Files in review order.
    pub files: Vec<ReviewFile>,
    /// Total added lines.
    pub additions: u32,
    /// Total removed lines.
    pub deletions: u32,
}

/// A changed file in a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    /// Old path for deleted/renamed files.
    pub old_path: Option<String>,
    /// New path for added/modified/renamed files.
    pub new_path: Option<String>,
    /// File status.
    pub status: ReviewFileStatus,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
    /// Parsed hunks.
    pub hunks: Vec<ReviewHunk>,
    /// Whether Git reported a binary patch.
    pub is_binary: bool,
}

impl ReviewFile {
    /// Return display path for the file.
    #[must_use]
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }
}

/// File status in a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    /// Modified file.
    Modified,
    /// Added file.
    Added,
    /// Deleted file.
    Deleted,
    /// Renamed file.
    Renamed,
    /// Copied file.
    Copied,
    /// Unknown or unsupported status.
    Unknown,
}

/// Unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewHunk {
    /// Old start line.
    pub old_start: u32,
    /// Old line count.
    pub old_count: u32,
    /// New start line.
    pub new_start: u32,
    /// New line count.
    pub new_count: u32,
    /// Optional hunk heading.
    pub heading: Option<String>,
    /// Diff lines in this hunk.
    pub lines: Vec<ReviewLine>,
}

/// A line in a unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewLine {
    /// Line kind.
    pub kind: ReviewLineKind,
    /// Old file line number, when present.
    pub old_line: Option<u32>,
    /// New file line number, when present.
    pub new_line: Option<u32>,
    /// Line content without the leading unified diff marker.
    pub content: String,
}

/// Diff line kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLineKind {
    /// Context line.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
}

#[derive(Debug, Error)]
enum ReviewError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("Git command failed: {0}")]
    Git(String),
    #[error("failed to parse diff: {0}")]
    Parse(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

fn create_review(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<CreateReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    match create_review_summary(&request) {
        Ok(summary) => json_response(&summary),
        Err(error) => ServiceResponse::error("review_failed", error.to_string()),
    }
}

fn create_review_summary(request: &CreateReviewRequest) -> Result<ReviewSummary, ReviewError> {
    if !request.repo_path.is_dir() {
        return Err(ReviewError::InvalidRequest(format!(
            "repo_path is not a directory: {}",
            request.repo_path.display()
        )));
    }

    let repo_root = git_output(&request.repo_path, &["rev-parse", "--show-toplevel"])?;
    let repo_root = PathBuf::from(repo_root.trim());
    let diff = diff_for_target(&repo_root, &request.target)?;
    let files = parse_unified_diff(&diff)?;
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();

    Ok(ReviewSummary {
        title: target_title(&request.target),
        repo_root,
        files,
        additions,
        deletions,
    })
}

fn diff_for_target(repo_root: &Path, target: &ReviewTarget) -> Result<String, ReviewError> {
    match target {
        ReviewTarget::WorkingTreeUnstaged => git_output(repo_root, &["diff", "--find-renames"]),
        ReviewTarget::IndexStaged => git_output(repo_root, &["diff", "--cached", "--find-renames"]),
        ReviewTarget::WorkingTreeAndIndex => {
            git_output(repo_root, &["diff", "HEAD", "--find-renames"])
        }
        ReviewTarget::LastCommit => {
            git_output(repo_root, &["show", "--format=", "--find-renames", "HEAD"])
        }
        ReviewTarget::CommitRange {
            base,
            head,
            merge_base,
        } => git_output(
            repo_root,
            &[
                "diff",
                "--find-renames",
                &range_spec(base, head, *merge_base),
            ],
        ),
        ReviewTarget::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => git_output(
            repo_root,
            &[
                "diff",
                "--find-renames",
                &range_spec(base_branch, head_branch, *merge_base),
            ],
        ),
    }
}

fn target_title(target: &ReviewTarget) -> String {
    match target {
        ReviewTarget::WorkingTreeUnstaged => "Unstaged Changes".to_string(),
        ReviewTarget::IndexStaged => "Staged Changes".to_string(),
        ReviewTarget::WorkingTreeAndIndex => "Staged + Unstaged Changes".to_string(),
        ReviewTarget::LastCommit => "Last Commit".to_string(),
        ReviewTarget::CommitRange {
            base,
            head,
            merge_base,
        } => format!("{base}{}{head}", if *merge_base { "..." } else { ".." }),
        ReviewTarget::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => format!(
            "{base_branch}{}{head_branch}",
            if *merge_base { "..." } else { ".." }
        ),
    }
}

fn range_spec(base: &str, head: &str, merge_base: bool) -> String {
    format!("{base}{}{head}", if merge_base { "..." } else { ".." })
}

fn git_output(repo_root: &Path, args: &[&str]) -> Result<String, ReviewError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    Err(ReviewError::Git(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

fn parse_unified_diff(diff: &str) -> Result<Vec<ReviewFile>, ReviewError> {
    let mut files = Vec::new();
    let mut current_file: Option<ReviewFile> = None;
    let mut current_hunk: Option<ReviewHunk> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            push_hunk(&mut current_file, current_hunk.take());
            push_file(&mut files, current_file.take());
            current_file = Some(file_from_diff_git_line(line));
            continue;
        }

        let Some(file) = current_file.as_mut() else {
            continue;
        };

        if line.starts_with("new file mode ") {
            file.status = ReviewFileStatus::Added;
            continue;
        }
        if line.starts_with("deleted file mode ") {
            file.status = ReviewFileStatus::Deleted;
            continue;
        }
        if line.starts_with("similarity index ") {
            file.status = ReviewFileStatus::Renamed;
            continue;
        }
        if let Some(path) = line.strip_prefix("rename from ") {
            file.old_path = Some(path.to_string());
            file.status = ReviewFileStatus::Renamed;
            continue;
        }
        if let Some(path) = line.strip_prefix("rename to ") {
            file.new_path = Some(path.to_string());
            file.status = ReviewFileStatus::Renamed;
            continue;
        }
        if line.starts_with("Binary files ") {
            file.is_binary = true;
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            if path != "/dev/null" {
                file.old_path = Some(strip_diff_path_prefix(path).to_string());
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            if path != "/dev/null" {
                file.new_path = Some(strip_diff_path_prefix(path).to_string());
            }
            continue;
        }
        if line.starts_with("@@ ") {
            push_hunk(&mut current_file, current_hunk.take());
            let hunk = parse_hunk_header(line)?;
            old_line = hunk.old_start;
            new_line = hunk.new_start;
            current_hunk = Some(hunk);
            continue;
        }

        let Some(hunk) = current_hunk.as_mut() else {
            continue;
        };

        if let Some(content) = line.strip_prefix('+') {
            hunk.lines.push(ReviewLine {
                kind: ReviewLineKind::Added,
                old_line: None,
                new_line: Some(new_line),
                content: content.to_string(),
            });
            file.additions = file.additions.saturating_add(1);
            new_line = new_line.saturating_add(1);
        } else if let Some(content) = line.strip_prefix('-') {
            hunk.lines.push(ReviewLine {
                kind: ReviewLineKind::Removed,
                old_line: Some(old_line),
                new_line: None,
                content: content.to_string(),
            });
            file.deletions = file.deletions.saturating_add(1);
            old_line = old_line.saturating_add(1);
        } else if let Some(content) = line.strip_prefix(' ') {
            hunk.lines.push(ReviewLine {
                kind: ReviewLineKind::Context,
                old_line: Some(old_line),
                new_line: Some(new_line),
                content: content.to_string(),
            });
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
        }
    }

    push_hunk(&mut current_file, current_hunk.take());
    push_file(&mut files, current_file.take());
    Ok(files)
}

fn file_from_diff_git_line(line: &str) -> ReviewFile {
    let rest = line.strip_prefix("diff --git ").unwrap_or_default();
    let mut parts = rest.split_whitespace();
    let old_path = parts.next().map(strip_diff_path_prefix).map(str::to_string);
    let new_path = parts.next().map(strip_diff_path_prefix).map(str::to_string);

    ReviewFile {
        old_path,
        new_path,
        status: ReviewFileStatus::Modified,
        additions: 0,
        deletions: 0,
        hunks: Vec::new(),
        is_binary: false,
    }
}

fn parse_hunk_header(line: &str) -> Result<ReviewHunk, ReviewError> {
    let Some(rest) = line.strip_prefix("@@ ") else {
        return Err(ReviewError::Parse(format!("invalid hunk header: {line}")));
    };
    let Some((ranges, heading)) = rest.split_once(" @@") else {
        return Err(ReviewError::Parse(format!("invalid hunk header: {line}")));
    };
    let mut ranges = ranges.split_whitespace();
    let old_range = ranges
        .next()
        .ok_or_else(|| ReviewError::Parse(format!("missing old range: {line}")))?;
    let new_range = ranges
        .next()
        .ok_or_else(|| ReviewError::Parse(format!("missing new range: {line}")))?;
    let (old_start, old_count) = parse_hunk_range(old_range, '-')?;
    let (new_start, new_count) = parse_hunk_range(new_range, '+')?;
    let heading = heading.trim();

    Ok(ReviewHunk {
        old_start,
        old_count,
        new_start,
        new_count,
        heading: (!heading.is_empty()).then(|| heading.to_string()),
        lines: Vec::new(),
    })
}

fn parse_hunk_range(range: &str, prefix: char) -> Result<(u32, u32), ReviewError> {
    let Some(range) = range.strip_prefix(prefix) else {
        return Err(ReviewError::Parse(format!(
            "hunk range missing '{prefix}' prefix: {range}"
        )));
    };
    let (start, count) = range.split_once(',').map_or((range, "1"), |parts| parts);
    let start = start
        .parse::<u32>()
        .map_err(|error| ReviewError::Parse(format!("invalid hunk start '{start}': {error}")))?;
    let count = count
        .parse::<u32>()
        .map_err(|error| ReviewError::Parse(format!("invalid hunk count '{count}': {error}")))?;
    Ok((start, count))
}

fn push_hunk(file: &mut Option<ReviewFile>, hunk: Option<ReviewHunk>) {
    if let (Some(file), Some(hunk)) = (file, hunk) {
        file.hunks.push(hunk);
    }
}

fn push_file(files: &mut Vec<ReviewFile>, file: Option<ReviewFile>) {
    if let Some(file) = file {
        files.push(file);
    }
}

fn strip_diff_path_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

const fn default_true() -> bool {
    true
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(CodeReviewPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(CodeReviewPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_file_diff() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n line one\n-old\n+new\n+extra\n";

        let files = parse_unified_diff(diff).expect("parse diff");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].display_path(), "src/lib.rs");
        assert_eq!(files[0].status, ReviewFileStatus::Modified);
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 4);
        assert_eq!(files[0].hunks[0].lines[0].old_line, Some(1));
        assert_eq!(files[0].hunks[0].lines[0].new_line, Some(1));
    }

    #[test]
    fn parses_rename_diff() {
        let diff = "diff --git a/old.rs b/new.rs\nsimilarity index 90%\nrename from old.rs\nrename to new.rs\n--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-old\n+new\n";

        let files = parse_unified_diff(diff).expect("parse diff");

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[0].new_path.as_deref(), Some("new.rs"));
        assert_eq!(files[0].status, ReviewFileStatus::Renamed);
        assert_eq!(files[0].additions, 1);
        assert_eq!(files[0].deletions, 1);
    }

    #[test]
    fn parses_single_line_hunk_range() {
        let hunk = parse_hunk_header("@@ -4 +4,2 @@ fn main()").expect("parse hunk");

        assert_eq!(hunk.old_start, 4);
        assert_eq!(hunk.old_count, 1);
        assert_eq!(hunk.new_start, 4);
        assert_eq!(hunk.new_count, 2);
        assert_eq!(hunk.heading.as_deref(), Some("fn main()"));
    }
}
