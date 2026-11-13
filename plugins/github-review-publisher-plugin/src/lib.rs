#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! GitHub review publisher plugin for Bcode.
//!
//! Serves three interfaces as thin adapters over `bcode_code_review_github`, which the review
//! itself links and calls in-process: the review publisher (`bcode.review_publisher/v1`, which
//! submits through the provider's `review_source.review.submit`), the read-only review importer
//! (`bcode.review_importer/v1`, which reads the provider's `review_source.pulls.activity`), and
//! the review-source provider (`bcode.review_source/v1`) itself.

use bcode_code_review_github::{GitHubReviewSource, review_draft};
use bcode_code_review_models::{
    DraftAnchor, ExternalAnchorMappingStatus, ExternalPublishReviewRequest,
    ExternalReviewAnchorMapping, ExternalReviewComment, ExternalReviewIdentity,
    ExternalReviewImportRequest, ExternalReviewImportResponse, ExternalReviewThread,
    ExternalReviewThreadState, OP_REVIEW_IMPORTER_IMPORT, OP_REVIEW_IMPORTER_MANIFEST,
    OP_REVIEW_PUBLISHER_MANIFEST, OP_REVIEW_PUBLISHER_PREVIEW, OP_REVIEW_PUBLISHER_SUBMIT,
    PublishReviewPreviewResponse, PublishReviewResponse, PullLocator, REVIEW_IMPORTER_INTERFACE_ID,
    REVIEW_PUBLISHER_INTERFACE_ID, REVIEW_SOURCE_INTERFACE_ID, RemoteAnchor, RemoteThread,
    ReviewAnchorKind, ReviewBundle, ReviewImporterManifest, ReviewPublisherCapabilities,
    ReviewPublisherManifest, ReviewSourcePullsActivityRequest, ReviewSubmitRequest, ReviewVerdict,
};
use bcode_code_review_provider::{ProviderCallError, ReviewSourceProvider};
use bcode_plugin_sdk::prelude::*;
use serde::Serialize;
use std::env;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use thiserror::Error;

const PUBLISHER_ID: &str = "github_pr_review";
const DEFAULT_SUBMIT_EVENT: &str = "COMMENT";

/// GitHub review publisher plugin.
#[derive(Default)]
pub struct GitHubReviewPublisherPlugin;

impl RustPlugin for GitHubReviewPublisherPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            REVIEW_PUBLISHER_INTERFACE_ID => match context.request.operation.as_str() {
                OP_REVIEW_PUBLISHER_MANIFEST => json_response(&github_manifest()),
                OP_REVIEW_PUBLISHER_PREVIEW => preview(&context.request),
                OP_REVIEW_PUBLISHER_SUBMIT => submit(&context.request),
                _ => ServiceResponse::error(
                    "unsupported_operation",
                    "unsupported GitHub publisher operation",
                ),
            },
            REVIEW_IMPORTER_INTERFACE_ID => match context.request.operation.as_str() {
                OP_REVIEW_IMPORTER_MANIFEST => json_response(&github_importer_manifest()),
                OP_REVIEW_IMPORTER_IMPORT => import(&context.request),
                _ => ServiceResponse::error(
                    "unsupported_operation",
                    "unsupported GitHub importer operation",
                ),
            },
            REVIEW_SOURCE_INTERFACE_ID => match bcode_code_review_provider::serve_review_source(
                &GitHubReviewSource,
                &context.request.operation,
                &context.request.payload,
            ) {
                Ok(payload) => ServiceResponse::ok(payload),
                Err(error) => ServiceResponse::error(error.code, error.message),
            },
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported GitHub review interface",
            ),
        }
    }
}

fn github_importer_manifest() -> ReviewImporterManifest {
    ReviewImporterManifest {
        id: PUBLISHER_ID.to_string(),
        label: "GitHub PR review".to_string(),
        description: "Import existing GitHub pull request review threads read-only".to_string(),
        options_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "repository": { "type": "string", "description": "GitHub repository, owner/repo. Defaults to gh repo view, then origin remote." },
                "pull_request": { "type": "string", "description": "Pull request number. Defaults to gh pr view, then branch patterns like pr/123." }
            }
        }),
    }
}

fn import(request: &ServiceRequest) -> ServiceResponse {
    let request: ExternalReviewImportRequest = match serde_json::from_slice(&request.payload) {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match import_for_request(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("github_import_failed", error.to_string()),
    }
}

fn import_for_request(
    request: &ExternalReviewImportRequest,
) -> Result<ExternalReviewImportResponse, GitHubPublisherError> {
    let options = GitHubPublishOptions::for_import(
        &request.options,
        &request.repo_root,
        request.pull_request.as_ref(),
    )?;
    let repository = options.resolve_repository()?;
    let number = options.resolve_pull_request()?;
    let activity = GitHubReviewSource.get_activity(&ReviewSourcePullsActivityRequest {
        repo_path: request.repo_root.clone(),
        repository: Some(repository.value),
        numbers: vec![number.value],
    })?;
    let threads = activity
        .activities
        .into_iter()
        .find(|activity| activity.number == number.value)
        .ok_or(GitHubPublisherError::MissingPullRequest)?
        .threads;
    Ok(import_threads(request, &threads))
}

/// GitHub's threads as imported threads, mapped onto the request's files.
fn import_threads(
    request: &ExternalReviewImportRequest,
    threads: &[RemoteThread],
) -> ExternalReviewImportResponse {
    let mut warnings = Vec::new();
    let threads = threads
        .iter()
        .map(|thread| {
            let mut mapping = map_anchor(request, &thread.anchor);
            if thread.outdated && mapping.status == ExternalAnchorMappingStatus::Exact {
                mapping.status = ExternalAnchorMappingStatus::Approximate;
                mapping.message = Some(
                    "GitHub marks this thread outdated; the anchor matches historical coordinates"
                        .to_string(),
                );
            }
            if mapping.status == ExternalAnchorMappingStatus::Unmappable {
                warnings.push(format!(
                    "GitHub thread {} at {} could not be mapped to the current review",
                    thread.id, thread.anchor.path
                ));
            }
            let state = if thread.outdated || thread.anchor.line.is_none() {
                ExternalReviewThreadState::Outdated
            } else if thread.resolved {
                ExternalReviewThreadState::Resolved
            } else {
                ExternalReviewThreadState::Open
            };
            ExternalReviewThread {
                identity: github_identity(
                    "thread",
                    &thread.id,
                    thread
                        .comments
                        .first()
                        .and_then(|comment| comment.url.clone()),
                ),
                state,
                mapping,
                comments: thread
                    .comments
                    .iter()
                    .map(|comment| ExternalReviewComment {
                        identity: github_identity("comment", &comment.id, comment.url.clone()),
                        author: Some(github_identity(
                            "user",
                            &comment.author.login,
                            comment.author.url.clone(),
                        )),
                        author_label: Some(comment.author.login.clone()),
                        body: comment.body.clone(),
                        created_at_ms: comment.created_at_ms,
                        updated_at_ms: comment.updated_at_ms,
                    })
                    .collect(),
            }
        })
        .collect();
    ExternalReviewImportResponse {
        importer_id: PUBLISHER_ID.to_string(),
        threads,
        warnings,
    }
}

fn github_identity(kind: &str, id: &str, url: Option<String>) -> ExternalReviewIdentity {
    ExternalReviewIdentity {
        provider_id: bcode_code_review_github::PROVIDER_ID.to_string(),
        external_id: format!("{kind}:{id}"),
        url,
    }
}

/// Map a thread's anchor onto the one line of the request's files it names.
fn map_anchor(
    request: &ExternalReviewImportRequest,
    anchor: &RemoteAnchor,
) -> ExternalReviewAnchorMapping {
    let unmappable = |message: &str| ExternalReviewAnchorMapping {
        status: ExternalAnchorMappingStatus::Unmappable,
        anchor: None,
        message: Some(message.to_string()),
    };
    let Some(file) = request
        .files
        .iter()
        .find(|file| file.display_path() == anchor.path)
    else {
        return unmappable("file is not present in the current review");
    };
    // Where GitHub places it now, else where it was written (an outdated thread).
    let Some(line) = anchor.line.or(anchor.original_line) else {
        return unmappable("GitHub marks this diff position as outdated");
    };
    let old_side = anchor.side == bcode_code_review_models::DiffSide::Old;
    let matching: Vec<_> = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|candidate| {
            if old_side {
                candidate.old_line == Some(line)
            } else {
                candidate.new_line == Some(line)
            }
        })
        .collect();
    let [only] = matching.as_slice() else {
        return unmappable("diff line is absent or ambiguous in the current review");
    };
    let start = anchor
        .start_line
        .or(anchor.original_start_line)
        .or(Some(line));
    let start_old = anchor.start_side.map_or(old_side, |side| {
        side == bcode_code_review_models::DiffSide::Old
    });
    let (old_line, new_line) = if old_side {
        (Some(line), None)
    } else {
        (None, Some(line))
    };
    let (old_start, new_start) = if start_old {
        (start, None)
    } else {
        (None, start)
    };
    ExternalReviewAnchorMapping {
        status: ExternalAnchorMappingStatus::Exact,
        anchor: Some(DraftAnchor {
            kind: ReviewAnchorKind::Range,
            file_path: anchor.path.clone(),
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start,
            old_end: old_line,
            new_start,
            new_end: new_line,
            old_line,
            new_line,
            line_kind: only.kind,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
            head_oid: None,
            blob_oid: None,
            reply_to: None,
        }),
        message: None,
    }
}

fn github_manifest() -> ReviewPublisherManifest {
    ReviewPublisherManifest {
        pull_request_provider: Some(bcode_code_review_github::PROVIDER_ID.to_string()),
        id: PUBLISHER_ID.to_string(),
        label: "GitHub PR review".to_string(),
        description: "Publish review comments to a GitHub pull request".to_string(),
        capabilities: ReviewPublisherCapabilities {
            preview: true,
            submit: true,
            update_existing: false,
            supports_threads: true,
            supports_ranges: true,
            supports_inline_comments: true,
            supports_summary_comment: true,
        },
        options_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "repository": { "type": "string", "description": "GitHub repository, owner/repo. Defaults to gh repo view, then origin remote." },
                "pull_request": { "type": "string", "description": "Pull request number. Defaults to gh pr view, then branch patterns like pr/123." },
                "submit_event": { "type": "string", "description": "GitHub review event", "default": "COMMENT", "enum": ["COMMENT", "REQUEST_CHANGES", "APPROVE"] },
                "summary": { "type": "string", "description": "Optional review summary body" },
                "fallback_file_comments_to_summary": { "type": "boolean", "description": "Summarize file/context comments instead of failing inline submit", "default": true },
                "fallback_unmapped_to_summary": { "type": "boolean", "description": "Include unmappable inline comments in the review summary instead of failing submit", "default": false }
            }
        }),
        route: None,
    }
}

fn preview(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ExternalPublishReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match preview_for_request(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("github_preview_failed", error.to_string()),
    }
}

fn submit(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<ExternalPublishReviewRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match submit_for_request(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("github_submit_failed", error.to_string()),
    }
}

/// The provider request publishing `bundle` with `options` to `repository`'s pull request
/// `number`.
fn submit_request(
    options: &GitHubPublishOptions,
    bundle: &ReviewBundle,
    repository: &str,
    number: u64,
) -> ReviewSubmitRequest {
    let verdict = options.verdict();
    let mut body = options.summary.clone().unwrap_or_default();
    // A comment-only review needs a body when it has nothing else.
    if body.is_empty() && verdict != ReviewVerdict::Approve {
        body = "Bcode review".to_string();
    }
    ReviewSubmitRequest {
        pull: PullLocator {
            repo_path: bundle.repo_root.clone(),
            repository: Some(repository.to_string()),
            number,
        },
        verdict,
        body,
        head_oid: None,
        bundle: Some(bundle.clone()),
        replies: Vec::new(),
        unplaced_into_body: options.fallback_unmapped_to_summary,
    }
}

fn preview_for_request(
    request: &ExternalPublishReviewRequest,
) -> Result<PublishReviewPreviewResponse, GitHubPublisherError> {
    let options = GitHubPublishOptions::from_json(&request.options, &request.bundle)?;
    let repository = options.resolve_repository()?;
    let pull_request = options.resolve_pull_request()?;
    let draft = review_draft(&request.bundle);
    let provider = GitHubReviewSource.preview_review(&submit_request(
        &options,
        &request.bundle,
        &repository.value,
        pull_request.value,
    ))?;
    let mut output = String::new();
    output.push_str("# GitHub PR review preview\n\n");
    let _ = writeln!(
        output,
        "* Repository: `{}` ({})",
        repository.value, repository.source
    );
    let _ = writeln!(
        output,
        "* Pull request: `#{}` ({})",
        pull_request.value, pull_request.source
    );
    let _ = writeln!(output, "* Auth: {}", options.auth_source_for_preview());
    let _ = writeln!(output, "* Event: `{}`", options.submit_event);
    let _ = writeln!(output, "* Inline comments: `{}`", draft.inline.len());
    let _ = write!(
        output,
        "* Repository comments in summary: `{}`\n\n",
        draft.summarized.len()
    );
    if !draft.unplaced.is_empty() || draft.skipped_resolved > 0 {
        output.push_str("## Warnings\n\n");
        for unplaced in &draft.unplaced {
            let _ = writeln!(output, "* not on a diff line: {unplaced}");
        }
        if draft.skipped_resolved > 0 {
            let _ = writeln!(
                output,
                "* skipped {} resolved Bcode review thread{}",
                draft.skipped_resolved,
                if draft.skipped_resolved == 1 { "" } else { "s" }
            );
        }
        output.push('\n');
    }
    output.push_str(&provider.preview);
    Ok(PublishReviewPreviewResponse {
        publisher_id: PUBLISHER_ID.to_string(),
        preview: output,
    })
}

fn submit_for_request(
    request: &ExternalPublishReviewRequest,
) -> Result<PublishReviewResponse, GitHubPublisherError> {
    let options = GitHubPublishOptions::from_json(&request.options, &request.bundle)?;
    let repository = options.resolve_repository()?;
    let pull_request = options.resolve_pull_request()?;
    let draft = review_draft(&request.bundle);
    if !draft.summarized.is_empty() && !options.fallback_file_comments_to_summary {
        return Err(GitHubPublisherError::UnmappableComments(draft.summarized));
    }
    let response = GitHubReviewSource.submit_review(&submit_request(
        &options,
        &request.bundle,
        &repository.value,
        pull_request.value,
    ))?;
    Ok(PublishReviewResponse {
        publisher_id: PUBLISHER_ID.to_string(),
        submitted: true,
        output: response.url,
        message: format!(
            "published {} review comments to GitHub PR #{}",
            draft.inline.len(),
            pull_request.value
        ),
    })
}

/// Repository and pull request from the options, else from the request's pull request target
/// when it belongs to this provider. Options always win.
fn target_options(
    value: &serde_json::Value,
    target: Option<&bcode_code_review_models::ReviewPullRequestTarget>,
) -> Result<(Option<String>, Option<u64>), GitHubPublisherError> {
    let target = target.filter(|target| target.provider == bcode_code_review_github::PROVIDER_ID);
    let repository = string_option(value, "repository")
        .or_else(|| target.map(|target| target.repository.clone()));
    if repository
        .as_ref()
        .is_some_and(|repository| !repository.contains('/'))
    {
        return Err(GitHubPublisherError::InvalidOption(
            "repository must be owner/repo".to_string(),
        ));
    }
    let pull_request = string_option(value, "pull_request")
        .map(|pull_request| {
            pull_request
                .parse::<u64>()
                .map_err(|error| GitHubPublisherError::InvalidOption(error.to_string()))
        })
        .transpose()?
        .or_else(|| target.map(|target| target.number));
    Ok((repository, pull_request))
}

#[derive(Debug, Clone)]
struct GitHubPublishOptions {
    repository: Option<String>,
    pull_request: Option<u64>,
    repo_root: std::path::PathBuf,
    submit_event: String,
    summary: Option<String>,
    fallback_file_comments_to_summary: bool,
    fallback_unmapped_to_summary: bool,
}

impl GitHubPublishOptions {
    fn from_json(
        value: &serde_json::Value,
        bundle: &ReviewBundle,
    ) -> Result<Self, GitHubPublisherError> {
        let mut options = Self::for_import(value, &bundle.repo_root, bundle.pull_request.as_ref())?;
        let submit_event = string_option(value, "submit_event")
            .unwrap_or_else(|| DEFAULT_SUBMIT_EVENT.to_string())
            .to_uppercase();
        if !matches!(
            submit_event.as_str(),
            "COMMENT" | "REQUEST_CHANGES" | "APPROVE"
        ) {
            return Err(GitHubPublisherError::InvalidOption(
                "submit_event must be COMMENT, REQUEST_CHANGES, or APPROVE".to_string(),
            ));
        }
        options.submit_event = submit_event;
        options.summary = string_option(value, "summary");
        options.fallback_file_comments_to_summary =
            string_option(value, "fallback_file_comments_to_summary")
                .is_none_or(|value| matches_bool_true(&value));
        options.fallback_unmapped_to_summary = bool_option(value, "fallback_unmapped_to_summary");
        Ok(options)
    }

    /// The options an import reads (the target only).
    fn for_import(
        value: &serde_json::Value,
        repo_root: &Path,
        target: Option<&bcode_code_review_models::ReviewPullRequestTarget>,
    ) -> Result<Self, GitHubPublisherError> {
        let (repository, pull_request) = target_options(value, target)?;
        Ok(Self {
            repository,
            pull_request,
            repo_root: repo_root.to_path_buf(),
            submit_event: DEFAULT_SUBMIT_EVENT.to_string(),
            summary: None,
            fallback_file_comments_to_summary: true,
            fallback_unmapped_to_summary: false,
        })
    }

    /// The neutral verdict of the GitHub event the options name.
    fn verdict(&self) -> ReviewVerdict {
        match self.submit_event.as_str() {
            "APPROVE" => ReviewVerdict::Approve,
            "REQUEST_CHANGES" => ReviewVerdict::RequestChanges,
            _ => ReviewVerdict::Comment,
        }
    }

    fn resolve_repository(&self) -> Result<ResolvedValue<String>, GitHubPublisherError> {
        if let Some(repository) = &self.repository {
            return Ok(ResolvedValue::new(repository.clone(), "options"));
        }
        if let Some(repository) = gh_output(
            &[
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "-q",
                ".nameWithOwner",
            ],
            &self.repo_root,
        )
        .ok()
        .filter(|value| !value.is_empty())
        {
            return Ok(ResolvedValue::new(repository, "gh repo view"));
        }
        let remote = command_output("git", &["remote", "get-url", "origin"], &self.repo_root)?;
        parse_github_remote_url(&remote)
            .map(|repository| ResolvedValue::new(repository, "origin remote"))
            .ok_or(GitHubPublisherError::MissingRepository)
    }

    fn resolve_pull_request(&self) -> Result<ResolvedValue<u64>, GitHubPublisherError> {
        if let Some(pull_request) = self.pull_request {
            return Ok(ResolvedValue::new(pull_request, "options"));
        }
        if let Some(pull_request) = gh_output(
            &["pr", "view", "--json", "number", "-q", ".number"],
            &self.repo_root,
        )
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        {
            return Ok(ResolvedValue::new(pull_request, "gh pr view"));
        }
        let branch = command_output("git", &["branch", "--show-current"], &self.repo_root)?;
        parse_pull_request_from_branch(&branch)
            .map(|pull_request| ResolvedValue::new(pull_request, "branch name"))
            .ok_or(GitHubPublisherError::MissingPullRequest)
    }

    /// Where submitting would take its credentials from (the provider's lookup order).
    fn auth_source_for_preview(&self) -> String {
        if env_token("GITHUB_TOKEN").is_some() {
            return "GITHUB_TOKEN env var".to_string();
        }
        if env_token("GH_TOKEN").is_some() {
            return "GH_TOKEN env var".to_string();
        }
        if gh_output(&["auth", "status"], &self.repo_root).is_ok() {
            return "GitHub CLI".to_string();
        }
        "unresolved; submit requires GITHUB_TOKEN, GH_TOKEN, or gh auth login".to_string()
    }
}

#[derive(Debug, Clone)]
struct ResolvedValue<T> {
    value: T,
    source: &'static str,
}

impl<T> ResolvedValue<T> {
    const fn new(value: T, source: &'static str) -> Self {
        Self { value, source }
    }
}

fn env_token(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn gh_output(args: &[&str], cwd: &Path) -> Result<String, GitHubPublisherError> {
    command_output("gh", args, cwd)
}

fn command_output(
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<String, GitHubPublisherError> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(GitHubPublisherError::Command)?;
    if !output.status.success() {
        return Err(GitHubPublisherError::CommandFailed {
            program: program.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn parse_github_remote_url(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches(".git");
    if let Some(path) = trimmed.strip_prefix("git@github.com:") {
        return owner_repo_from_path(path);
    }
    if let Some(path) = trimmed.strip_prefix("https://github.com/") {
        return owner_repo_from_path(path);
    }
    if let Some(path) = trimmed.strip_prefix("ssh://git@github.com/") {
        return owner_repo_from_path(path);
    }
    None
}

fn owner_repo_from_path(path: &str) -> Option<String> {
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

fn parse_pull_request_from_branch(branch: &str) -> Option<u64> {
    branch
        .trim()
        .strip_prefix("pull/")
        .and_then(|rest| rest.split('/').next())
        .or_else(|| branch.trim().strip_prefix("pr/"))
        .and_then(|value| value.parse().ok())
}

fn string_option(value: &serde_json::Value, key: &'static str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

const fn matches_bool_true(value: &str) -> bool {
    value.eq_ignore_ascii_case("true")
}

fn bool_option(value: &serde_json::Value, key: &'static str) -> bool {
    value.get(key).is_some_and(|value| {
        value
            .as_bool()
            .unwrap_or_else(|| value.as_str().is_some_and(matches_bool_true))
    })
}

#[derive(Debug, Error)]
enum GitHubPublisherError {
    #[error(
        "missing GitHub repository; pass repository option or run from a GitHub repo with gh/git configured"
    )]
    MissingRepository,
    #[error(
        "missing GitHub pull request; pass pull_request option or run from a PR branch with gh/git configured"
    )]
    MissingPullRequest,
    #[error("invalid option: {0}")]
    InvalidOption(String),
    #[error("command failed to start: {0}")]
    Command(#[from] std::io::Error),
    #[error("command {program} failed: {stderr}")]
    CommandFailed { program: String, stderr: String },
    #[error("comments outside the diff (fallback_file_comments_to_summary is off): {0:?}")]
    UnmappableComments(Vec<String>),
    #[error("{0}")]
    Provider(#[from] ProviderCallError),
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
    bcode_plugin_sdk::static_plugin_vtable!(
        GitHubReviewPublisherPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(
    GitHubReviewPublisherPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{
        DiffSide, DraftComment, Participant, RemoteComment, ReviewBundleLine, ReviewBundleThread,
        ReviewLineKind, ReviewTarget, ReviewThreadKind, ReviewThreadSeverity,
    };
    use std::path::PathBuf;

    #[test]
    fn manifest_uses_external_publisher_interface() {
        let manifest = github_manifest();

        assert_eq!(manifest.id, PUBLISHER_ID);
        assert!(manifest.capabilities.supports_inline_comments);
        assert!(manifest.options_schema.get("properties").is_some());
    }

    #[test]
    fn parses_required_options() {
        let options = GitHubPublishOptions::from_json(
            &serde_json::json!({
                "repository": "owner/repo",
                "pull_request": "123"
            }),
            &empty_bundle(),
        )
        .expect("parse options");

        assert_eq!(options.repository.as_deref(), Some("owner/repo"));
        assert_eq!(options.pull_request, Some(123));
        assert_eq!(options.submit_event, DEFAULT_SUBMIT_EVENT);
        assert_eq!(options.verdict(), ReviewVerdict::Comment);
    }

    #[test]
    fn submit_events_map_to_neutral_verdicts() {
        let verdict = |event: &str| {
            GitHubPublishOptions::from_json(
                &serde_json::json!({ "submit_event": event }),
                &empty_bundle(),
            )
            .expect("options")
            .verdict()
        };
        assert_eq!(verdict("approve"), ReviewVerdict::Approve);
        assert_eq!(verdict("REQUEST_CHANGES"), ReviewVerdict::RequestChanges);
        assert_eq!(verdict("COMMENT"), ReviewVerdict::Comment);
        assert!(
            GitHubPublishOptions::from_json(
                &serde_json::json!({ "submit_event": "MERGE" }),
                &empty_bundle()
            )
            .is_err()
        );
    }

    #[test]
    fn submissions_carry_the_body_and_the_unplaced_policy() {
        let options = GitHubPublishOptions::from_json(
            &serde_json::json!({ "submit_event": "APPROVE", "fallback_unmapped_to_summary": true }),
            &empty_bundle(),
        )
        .expect("options");
        let request = submit_request(&options, &empty_bundle(), "owner/repo", 9);
        assert_eq!(request.verdict, ReviewVerdict::Approve);
        assert_eq!(request.body, "", "an approval needs no body");
        assert!(request.unplaced_into_body);
        assert_eq!(request.pull.repository.as_deref(), Some("owner/repo"));
        assert_eq!(request.pull.number, 9);

        let options = GitHubPublishOptions::from_json(&serde_json::json!({}), &empty_bundle())
            .expect("options");
        let request = submit_request(&options, &empty_bundle(), "owner/repo", 9);
        assert_eq!(request.body, "Bcode review");
        assert!(!request.unplaced_into_body);
    }

    #[test]
    fn importer_manifest_uses_read_only_import_interface() {
        let manifest = github_importer_manifest();

        assert_eq!(manifest.id, PUBLISHER_ID);
        assert!(manifest.description.contains("read-only"));
        assert_eq!(REVIEW_IMPORTER_INTERFACE_ID, "bcode.review_importer/v1");
        assert_eq!(OP_REVIEW_IMPORTER_IMPORT, "review.importer.import");
    }

    #[test]
    fn imported_thread_maps_exactly_to_current_diff_line() {
        let request = import_request(vec![added(42)]);
        let response = import_threads(&request, &[remote_thread("T1", Some(42), 1)]);

        assert_eq!(response.warnings, [] as [String; 0]);
        let thread = response.threads.first().expect("imported thread");
        assert_eq!(thread.state, ExternalReviewThreadState::Open);
        assert_eq!(thread.identity.external_id, "thread:T1");
        assert_eq!(thread.mapping.status, ExternalAnchorMappingStatus::Exact);
        assert_eq!(
            thread
                .mapping
                .anchor
                .as_ref()
                .and_then(|anchor| anchor.new_line),
            Some(42)
        );
        assert_eq!(thread.comments[0].author_label.as_deref(), Some("reviewer"));
        assert_eq!(thread.comments[0].created_at_ms, Some(1));
    }

    #[test]
    fn resolved_threads_import_resolved() {
        let request = import_request(vec![added(42)]);
        let mut thread = remote_thread("T1", Some(42), 1);
        thread.resolved = true;
        let response = import_threads(&request, &[thread]);

        assert_eq!(
            response.threads.first().expect("thread").state,
            ExternalReviewThreadState::Resolved
        );
    }

    #[test]
    fn outdated_state_takes_precedence_over_resolved() {
        let request = import_request(vec![added(42)]);
        let mut thread = remote_thread("T1", Some(42), 1);
        thread.resolved = true;
        thread.outdated = true;
        let response = import_threads(&request, &[thread]);

        let thread = response.threads.first().expect("thread");
        assert_eq!(thread.state, ExternalReviewThreadState::Outdated);
        assert_eq!(
            thread.mapping.status,
            ExternalAnchorMappingStatus::Approximate
        );
    }

    #[test]
    fn outdated_import_is_preserved_as_unmappable_without_losing_replies() {
        let request = import_request(Vec::new());
        let response = import_threads(&request, &[remote_thread("T1", None, 2)]);

        assert_eq!(response.threads.len(), 1);
        let thread = response.threads.first().expect("imported thread");
        assert_eq!(thread.state, ExternalReviewThreadState::Outdated);
        assert_eq!(
            thread.mapping.status,
            ExternalAnchorMappingStatus::Unmappable
        );
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(response.warnings.len(), 1);
    }

    fn added(line: u32) -> bcode_code_review_models::ReviewLine {
        bcode_code_review_models::ReviewLine {
            kind: ReviewLineKind::Added,
            old_line: None,
            new_line: Some(line),
            content: "added".to_string(),
        }
    }

    fn remote_thread(id: &str, line: Option<u32>, comments: u64) -> RemoteThread {
        RemoteThread {
            id: id.to_string(),
            anchor: RemoteAnchor {
                path: "src/lib.rs".to_string(),
                side: DiffSide::New,
                line,
                ..RemoteAnchor::default()
            },
            resolved: false,
            outdated: false,
            can_reply: true,
            can_resolve: true,
            comments: (1..=comments)
                .map(|index| RemoteComment {
                    id: format!("C{index}"),
                    author: Participant {
                        login: "reviewer".to_string(),
                        ..Participant::default()
                    },
                    body: format!("comment {index}"),
                    created_at_ms: Some(index),
                    updated_at_ms: Some(index),
                    url: Some(format!("https://github.test/comment/{index}")),
                    review_id: None,
                })
                .collect(),
        }
    }

    fn import_request(
        lines: Vec<bcode_code_review_models::ReviewLine>,
    ) -> ExternalReviewImportRequest {
        ExternalReviewImportRequest {
            pull_request: None,
            repo_root: PathBuf::from("/repo"),
            files: vec![bcode_code_review_models::ReviewFile {
                old_path: None,
                new_path: Some("src/lib.rs".to_string()),
                status: bcode_code_review_models::ReviewFileStatus::Modified,
                additions: 1,
                deletions: 0,
                hunks: vec![bcode_code_review_models::ReviewHunk {
                    old_start: 1,
                    old_count: 0,
                    new_start: 42,
                    new_count: 1,
                    heading: None,
                    lines,
                }],
                is_binary: false,
            }],
            options: serde_json::json!({}),
        }
    }

    #[test]
    fn preview_is_local_and_does_not_require_submit_auth_or_http() {
        let mut bundle = empty_bundle();
        bundle.threads = vec![review_thread(bundle_line(
            ReviewLineKind::Added,
            None,
            Some(42),
        ))];
        let request = ExternalPublishReviewRequest {
            bundle,
            options: serde_json::json!({
                "repository": "owner/repo",
                "pull_request": "123",
            }),
        };

        let response = preview_for_request(&request).expect("offline preview");

        assert_eq!(response.publisher_id, PUBLISHER_ID);
        assert!(response.preview.contains("GitHub PR review preview"));
        assert!(response.preview.contains("owner/repo"));
        assert!(response.preview.contains("`src/lib.rs` line 42"));
    }

    #[test]
    fn preview_targets_the_bundles_pull_request_and_summarizes_summary_only_threads() {
        let mut summary_only = review_thread(bundle_line(ReviewLineKind::Context, None, Some(7)));
        summary_only.summary_only = true;
        summary_only.thread_id = "whole-stack".to_string();
        let mut bundle = empty_bundle();
        bundle.pull_request = Some(bcode_code_review_models::ReviewPullRequestTarget {
            provider: bcode_code_review_github::PROVIDER_ID.to_string(),
            repository: "owner/stacked".to_string(),
            number: 42,
        });
        bundle.threads = vec![
            review_thread(bundle_line(ReviewLineKind::Added, None, Some(3))),
            summary_only,
        ];
        let request = ExternalPublishReviewRequest {
            bundle: bundle.clone(),
            options: serde_json::json!({}),
        };
        let response = preview_for_request(&request).expect("offline preview");
        assert!(
            response.preview.contains("`owner/stacked`"),
            "{}",
            response.preview
        );
        assert!(response.preview.contains("`#42`"), "{}", response.preview);
        assert!(response.preview.contains("Inline comments: `1`"));
        assert!(
            response
                .preview
                .contains("Repository comments in summary: `1`")
        );

        // Options still win over the target; a target of another provider is ignored.
        let request = ExternalPublishReviewRequest {
            bundle: bundle.clone(),
            options: serde_json::json!({ "repository": "owner/other", "pull_request": "7" }),
        };
        let response = preview_for_request(&request).expect("preview");
        assert!(response.preview.contains("`owner/other`") && response.preview.contains("`#7`"));
        bundle.pull_request.as_mut().unwrap().provider = "elsewhere".to_string();
        let options =
            GitHubPublishOptions::from_json(&serde_json::json!({}), &bundle).expect("options");
        assert_eq!((options.repository, options.pull_request), (None, None));
    }

    #[test]
    fn preview_reports_unmappable_and_resolved_comments() {
        let mut resolved = review_thread(bundle_line(ReviewLineKind::Added, None, Some(42)));
        resolved.resolved_at_ms = Some(123);
        let mut bundle = empty_bundle();
        bundle.threads = vec![
            review_thread(bundle_line(ReviewLineKind::Context, None, None)),
            resolved,
        ];
        let request = ExternalPublishReviewRequest {
            bundle,
            options: serde_json::json!({
                "repository": "owner/repo",
                "pull_request": "123"
            }),
        };

        let response = preview_for_request(&request).expect("preview");

        assert!(response.preview.contains("## Warnings"));
        assert!(response.preview.contains("not on a diff line"));
        assert!(response.preview.contains("skipped 1 resolved"));
        assert!(response.preview.contains("Inline comments: `0`"));
    }

    #[test]
    fn parses_fallback_unmapped_option() {
        let options = GitHubPublishOptions::from_json(
            &serde_json::json!({
                "repository": "owner/repo",
                "pull_request": "123",
                "fallback_unmapped_to_summary": "true"
            }),
            &empty_bundle(),
        )
        .expect("parse options");

        assert!(options.fallback_unmapped_to_summary);
    }

    #[test]
    fn parses_github_remote_urls() {
        assert_eq!(
            parse_github_remote_url("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_remote_url("https://github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn parses_pull_request_branch_names() {
        assert_eq!(parse_pull_request_from_branch("pull/123/head"), Some(123));
        assert_eq!(parse_pull_request_from_branch("pr/456"), Some(456));
    }

    fn empty_bundle() -> ReviewBundle {
        ReviewBundle {
            pull_request: None,
            review_id: "review".to_string(),
            title: "Review".to_string(),
            repo_root: PathBuf::from("/repo"),
            target: ReviewTarget::WorkingTreeUnstaged,
            surfaces: Vec::new(),
            files: Vec::new(),
            threads: Vec::new(),
            generated_at_ms: 1,
        }
    }

    fn bundle_line(
        kind: ReviewLineKind,
        old_line: Option<u32>,
        new_line: Option<u32>,
    ) -> ReviewBundleLine {
        ReviewBundleLine {
            file_path: "src/lib.rs".to_string(),
            kind,
            old_line,
            new_line,
            diff_row: 1,
            content: String::new(),
            surface_id: None,
            source_id: None,
        }
    }

    fn review_thread(line: ReviewBundleLine) -> ReviewBundleThread {
        let anchor = DraftAnchor {
            kind: ReviewAnchorKind::Range,
            file_path: line.file_path.clone(),
            diff_row: line.diff_row,
            start_diff_row: None,
            end_diff_row: None,
            old_start: None,
            old_end: None,
            new_start: line.new_line,
            new_end: line.new_line,
            old_line: line.old_line,
            new_line: line.new_line,
            line_kind: line.kind,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
            head_oid: None,
            blob_oid: None,
            reply_to: None,
        };
        ReviewBundleThread {
            summary_only: false,
            thread_id: "thread".to_string(),
            anchor: anchor.clone(),
            comments: vec![DraftComment {
                comment_id: "comment".to_string(),
                thread_id: "thread".to_string(),
                anchor,
                body: "comment body".to_string(),
                created_at_ms: 1,
                updated_at_ms: 1,
                session_id: None,
                resolved_at_ms: None,
                thread_kind: ReviewThreadKind::Note,
                severity: ReviewThreadSeverity::Info,
            }],
            session_id: None,
            resolved_at_ms: None,
            selected_lines: vec![line],
            selected_diff_lines: Vec::new(),
            hunk_context: Vec::new(),
        }
    }
}
