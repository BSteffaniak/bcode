#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! GitHub review publisher plugin for Bcode.

use bcode_code_review_models::{
    ExternalAnchorMappingStatus, ExternalPublishReviewRequest, ExternalReviewAnchorMapping,
    ExternalReviewComment, ExternalReviewIdentity, ExternalReviewImportRequest,
    ExternalReviewImportResponse, ExternalReviewThread, ExternalReviewThreadState,
    OP_REVIEW_IMPORTER_IMPORT, OP_REVIEW_IMPORTER_MANIFEST, OP_REVIEW_PUBLISHER_MANIFEST,
    OP_REVIEW_PUBLISHER_PREVIEW, OP_REVIEW_PUBLISHER_SUBMIT, PublishReviewPreviewResponse,
    PublishReviewResponse, REVIEW_IMPORTER_INTERFACE_ID, REVIEW_PUBLISHER_INTERFACE_ID,
    ReviewBundle, ReviewBundleLine, ReviewBundleThread, ReviewImporterManifest, ReviewLineKind,
    ReviewPublisherCapabilities, ReviewPublisherManifest,
};
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::env;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use thiserror::Error;

const PUBLISHER_ID: &str = "github_pr_review";
const DEFAULT_TOKEN_ENV: &str = "GITHUB_TOKEN";
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
                "pull_request": { "type": "string", "description": "Pull request number. Defaults to gh pr view, then branch patterns like pr/123." },
                "token_env": { "type": "string", "description": "GitHub token env var", "default": "GITHUB_TOKEN" }
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
    let options = GitHubImportOptions::from_json(&request.options, &request.repo_root)?;
    let resolved = options.resolve()?;
    let (imported, thread_states) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let comments = fetch_github_review_comments(
                &resolved.repository.value,
                resolved.pull_request.value,
                &resolved.auth.token,
            )
            .await?;
            let states = fetch_github_review_thread_states(
                &resolved.repository.value,
                resolved.pull_request.value,
                &resolved.auth.token,
            )
            .await?;
            Ok::<_, GitHubPublisherError>((comments, states))
        })?;
    Ok(map_github_review_comments(
        request,
        imported,
        &thread_states,
    ))
}

fn github_manifest() -> ReviewPublisherManifest {
    ReviewPublisherManifest {
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
                "token_env": { "type": "string", "description": "GitHub token env var", "default": "GITHUB_TOKEN" },
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

fn preview_for_request(
    request: &ExternalPublishReviewRequest,
) -> Result<PublishReviewPreviewResponse, GitHubPublisherError> {
    let options = GitHubPublishOptions::from_json(&request.options, &request.bundle)?;
    let repository = options.resolve_repository()?;
    let pull_request = options.resolve_pull_request()?;
    let auth_source = options.auth_source_for_preview();
    let draft = github_review_draft(&request.bundle, &options);
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
    let _ = writeln!(output, "* Auth: {auth_source}");
    let _ = writeln!(output, "* Event: `{}`", options.submit_event);
    let _ = writeln!(output, "* Inline comments: `{}`", draft.comments.len());
    let _ = write!(
        output,
        "* Repository comments in summary: `{}`\n\n",
        draft.summary_comments.len()
    );
    if let Some(summary) = &draft.body {
        output.push_str("## Summary\n\n");
        output.push_str(summary);
        output.push_str("\n\n");
    }
    if !draft.summary_comments.is_empty() {
        output.push_str("## Repository comments\n\n");
        for comment in &draft.summary_comments {
            let _ = writeln!(output, "* {comment}");
        }
        output.push('\n');
    }
    if !draft.warnings.is_empty() {
        output.push_str("## Warnings\n\n");
        for warning in &draft.warnings {
            let _ = writeln!(output, "* {warning}");
        }
        output.push('\n');
    }
    output.push_str("## Inline comments\n\n");
    for comment in &draft.comments {
        let _ = write!(
            output,
            "* `{}` line `{}` side `{}`\n\n{}\n\n",
            comment.path, comment.line, comment.side, comment.body
        );
    }
    Ok(PublishReviewPreviewResponse {
        publisher_id: PUBLISHER_ID.to_string(),
        preview: output,
    })
}

fn submit_for_request(
    request: &ExternalPublishReviewRequest,
) -> Result<PublishReviewResponse, GitHubPublisherError> {
    let options = GitHubPublishOptions::from_json(&request.options, &request.bundle)?;
    let resolved = options.resolve()?;
    let token = resolved.auth.token;
    let mut draft = github_review_draft(&request.bundle, &options);
    if !draft.warnings.is_empty() && !options.fallback_unmapped_to_summary {
        return Err(GitHubPublisherError::UnmappableComments(draft.warnings));
    }
    if !draft.summary_comments.is_empty() && !options.fallback_file_comments_to_summary {
        return Err(GitHubPublisherError::UnmappableComments(
            draft.summary_comments.clone(),
        ));
    }
    if (options.fallback_unmapped_to_summary && !draft.warnings.is_empty())
        || (options.fallback_file_comments_to_summary && !draft.summary_comments.is_empty())
    {
        append_unmapped_to_summary(&mut draft);
    }
    let payload = GitHubCreateReviewRequest {
        body: draft.body.unwrap_or_else(|| "Bcode review".to_string()),
        event: options.submit_event,
        comments: draft.comments,
    };
    let response = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(create_github_review(
            &resolved.repository.value,
            resolved.pull_request.value,
            &token,
            &payload,
        ))?;
    Ok(PublishReviewResponse {
        publisher_id: PUBLISHER_ID.to_string(),
        submitted: true,
        output: response.html_url,
        message: format!(
            "published {} review comments to GitHub PR #{}",
            payload.comments.len(),
            resolved.pull_request.value
        ),
    })
}

async fn create_github_review(
    repository: &str,
    pull_request: u64,
    token: &str,
    payload: &GitHubCreateReviewRequest,
) -> Result<GitHubCreateReviewResponse, GitHubPublisherError> {
    let url = format!("https://api.github.com/repos/{repository}/pulls/{pull_request}/reviews");
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "bcode-github-review-publisher")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(payload)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(GitHubPublisherError::Api(format!(
            "GitHub API returned {status}: {body}"
        )));
    }
    serde_json::from_str(&body).map_err(GitHubPublisherError::Json)
}

fn github_review_draft(bundle: &ReviewBundle, options: &GitHubPublishOptions) -> GitHubReviewDraft {
    let mut comments = Vec::new();
    let mut warnings = Vec::new();
    let mut summary_comments = Vec::new();
    let mut skipped_resolved_count = 0usize;
    for thread in &bundle.threads {
        if thread.resolved_at_ms.is_some() {
            skipped_resolved_count = skipped_resolved_count.saturating_add(1);
            continue;
        }
        if should_summarize_thread(thread) {
            summary_comments.push(summary_comment_for_thread(thread));
            continue;
        }
        match github_comment_for_thread(thread) {
            Some(mut comment) => {
                comment.body = thread_body(thread);
                comments.push(comment);
            }
            None => warnings.push(format!(
                "thread {} at {} could not be mapped to a GitHub review line",
                thread.thread_id, thread.anchor.file_path
            )),
        }
    }
    if skipped_resolved_count > 0 {
        warnings.push(format!(
            "skipped {skipped_resolved_count} resolved Bcode review thread{}",
            if skipped_resolved_count == 1 { "" } else { "s" }
        ));
    }
    GitHubReviewDraft {
        body: options.summary.clone(),
        comments,
        warnings,
        summary_comments,
    }
}

fn should_summarize_thread(thread: &ReviewBundleThread) -> bool {
    thread.anchor.is_file_anchor
        || thread
            .selected_lines
            .iter()
            .any(|line| line.surface_id.is_some() && line.kind == ReviewLineKind::Context)
}

fn summary_comment_for_thread(thread: &ReviewBundleThread) -> String {
    let line = thread
        .anchor
        .new_line
        .or(thread.anchor.new_start)
        .or(thread.anchor.old_line)
        .or(thread.anchor.old_start)
        .map_or_else(String::new, |line| format!(":{line}"));
    format!(
        "{}{} — {}",
        thread.anchor.file_path,
        line,
        thread_body(thread).replace('\n', " ")
    )
}

fn append_unmapped_to_summary(draft: &mut GitHubReviewDraft) {
    let mut body = draft
        .body
        .take()
        .unwrap_or_else(|| "Bcode review".to_string());
    body.push_str("\n\n## Unmapped Bcode comments\n\n");
    for warning in &draft.warnings {
        let _ = writeln!(body, "* {warning}");
    }
    if !draft.summary_comments.is_empty() {
        body.push_str("\n## Repository Bcode comments\n\n");
        for comment in &draft.summary_comments {
            let _ = writeln!(body, "* {comment}");
        }
    }
    draft.body = Some(body);
}

fn github_comment_for_thread(thread: &ReviewBundleThread) -> Option<GitHubReviewComment> {
    let line = best_anchor_line(thread)?;
    let side = side_for_line(&line)?.to_string();
    let line_number = github_line_number(&line)?;
    let range = github_range_for_thread(thread, &side);
    let (start_line, start_side) = range.map_or((None, None), |range| {
        (Some(range.start_line), Some(range.start_side))
    });
    Some(GitHubReviewComment {
        path: line.file_path,
        body: String::new(),
        line: line_number,
        side,
        start_line,
        start_side,
    })
}

fn github_range_for_thread(
    thread: &ReviewBundleThread,
    end_side: &str,
) -> Option<GitHubReviewRange> {
    let first = thread
        .selected_lines
        .iter()
        .find(|line| side_for_line(line).is_some_and(|side| side == end_side))?;
    let last = thread
        .selected_lines
        .iter()
        .rev()
        .find(|line| side_for_line(line).is_some_and(|side| side == end_side))?;
    let start_line = github_line_number(first)?;
    let end_line = github_line_number(last)?;
    (start_line != end_line).then(|| GitHubReviewRange {
        start_line,
        start_side: end_side.to_string(),
    })
}

fn side_for_line(line: &ReviewBundleLine) -> Option<&'static str> {
    match line.kind {
        ReviewLineKind::Added | ReviewLineKind::Context => line.new_line.map(|_| "RIGHT"),
        ReviewLineKind::Removed => line.old_line.map(|_| "LEFT"),
    }
}

const fn github_line_number(line: &ReviewBundleLine) -> Option<u32> {
    match line.kind {
        ReviewLineKind::Added | ReviewLineKind::Context => line.new_line,
        ReviewLineKind::Removed => line.old_line,
    }
}

fn best_anchor_line(thread: &ReviewBundleThread) -> Option<ReviewBundleLine> {
    thread
        .selected_lines
        .iter()
        .rev()
        .find(|line| line.new_line.is_some() || line.old_line.is_some())
        .cloned()
}

fn thread_body(thread: &ReviewBundleThread) -> String {
    let mut body = thread
        .comments
        .iter()
        .map(|comment| comment.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    if let Some(surface_id) = &thread.anchor.surface_id {
        let _ = write!(body, "\n\n_Bcode surface: `{surface_id}`_");
    }
    if let Some(source_id) = &thread.anchor.source_id {
        let _ = write!(body, "\n\n_Bcode source: `{source_id}`_");
    }
    if let Some(session_id) = &thread.session_id {
        let _ = write!(body, "\n\n_Bcode session: `{session_id}`_");
    }
    body
}

#[derive(Debug, Clone)]
struct GitHubImportOptions {
    repository: Option<String>,
    pull_request: Option<u64>,
    repo_root: std::path::PathBuf,
    token_env: String,
}

impl GitHubImportOptions {
    fn from_json(
        value: &serde_json::Value,
        repo_root: &Path,
    ) -> Result<Self, GitHubPublisherError> {
        let repository = string_option(value, "repository");
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
            .transpose()?;
        Ok(Self {
            repository,
            pull_request,
            repo_root: repo_root.to_path_buf(),
            token_env: string_option(value, "token_env")
                .unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_string()),
        })
    }

    fn resolve(&self) -> Result<ResolvedGitHubPublishOptions, GitHubPublisherError> {
        GitHubPublishOptions {
            repository: self.repository.clone(),
            pull_request: self.pull_request,
            repo_root: self.repo_root.clone(),
            token_env: self.token_env.clone(),
            submit_event: DEFAULT_SUBMIT_EVENT.to_string(),
            summary: None,
            fallback_file_comments_to_summary: false,
            fallback_unmapped_to_summary: false,
        }
        .resolve()
    }
}

#[derive(Debug, Clone)]
struct GitHubPublishOptions {
    repository: Option<String>,
    pull_request: Option<u64>,
    repo_root: std::path::PathBuf,
    token_env: String,
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
        let repository = string_option(value, "repository");
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
            .transpose()?;
        let token_env =
            string_option(value, "token_env").unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_string());
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
        Ok(Self {
            repository,
            pull_request,
            repo_root: bundle.repo_root.clone(),
            token_env,
            submit_event,
            summary: string_option(value, "summary"),
            fallback_file_comments_to_summary: string_option(
                value,
                "fallback_file_comments_to_summary",
            )
            .is_none_or(|value| matches_bool_true(&value)),
            fallback_unmapped_to_summary: bool_option(value, "fallback_unmapped_to_summary"),
        })
    }

    fn resolve(&self) -> Result<ResolvedGitHubPublishOptions, GitHubPublisherError> {
        Ok(ResolvedGitHubPublishOptions {
            repository: self.resolve_repository()?,
            pull_request: self.resolve_pull_request()?,
            auth: self.resolve_auth()?,
        })
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

    fn auth_source_for_preview(&self) -> String {
        if env_token(&self.token_env).is_some() {
            return format!("{} env var", self.token_env);
        }
        if self.token_env != DEFAULT_TOKEN_ENV && env_token(DEFAULT_TOKEN_ENV).is_some() {
            return "GITHUB_TOKEN env var".to_string();
        }
        if env_token("GH_TOKEN").is_some() {
            return "GH_TOKEN env var".to_string();
        }
        if gh_output(&["auth", "status"], &self.repo_root).is_ok() {
            return "GitHub CLI".to_string();
        }
        "unresolved; submit requires token_env, GITHUB_TOKEN, GH_TOKEN, or gh auth login"
            .to_string()
    }

    fn resolve_auth(&self) -> Result<ResolvedAuth, GitHubPublisherError> {
        if let Some(token) = env_token(&self.token_env) {
            return Ok(ResolvedAuth::new(token));
        }
        if self.token_env != DEFAULT_TOKEN_ENV
            && let Some(token) = env_token(DEFAULT_TOKEN_ENV)
        {
            return Ok(ResolvedAuth::new(token));
        }
        if let Some(token) = env_token("GH_TOKEN") {
            return Ok(ResolvedAuth::new(token));
        }
        let token = gh_output(&["auth", "token"], &self.repo_root)
            .map_err(|_| GitHubPublisherError::MissingToken)?;
        if token.trim().is_empty() {
            return Err(GitHubPublisherError::MissingToken);
        }
        Ok(ResolvedAuth::new(token))
    }
}

#[derive(Debug, Clone)]
struct ResolvedGitHubPublishOptions {
    repository: ResolvedValue<String>,
    pull_request: ResolvedValue<u64>,
    auth: ResolvedAuth,
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

#[derive(Debug, Clone)]
struct ResolvedAuth {
    token: String,
}

impl ResolvedAuth {
    const fn new(token: String) -> Self {
        Self { token }
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

#[derive(Debug)]
struct GitHubReviewDraft {
    body: Option<String>,
    comments: Vec<GitHubReviewComment>,
    warnings: Vec<String>,
    summary_comments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubReviewRange {
    start_line: u32,
    start_side: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GitHubCreateReviewRequest {
    body: String,
    event: String,
    comments: Vec<GitHubReviewComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GitHubReviewComment {
    path: String,
    body: String,
    line: u32,
    side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_side: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct GitHubCreateReviewResponse {
    html_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct GitHubReviewCommentResponse {
    id: u64,
    node_id: String,
    body: String,
    path: String,
    line: Option<u32>,
    original_line: Option<u32>,
    side: Option<String>,
    start_line: Option<u32>,
    original_start_line: Option<u32>,
    start_side: Option<String>,
    #[serde(default)]
    in_reply_to_id: Option<u64>,
    #[serde(default)]
    html_url: Option<String>,
    user: GitHubReviewUser,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct GitHubReviewUser {
    id: u64,
    login: String,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubReviewThreadState {
    thread_id: String,
    comment_ids: Vec<u64>,
    comment_node_ids: Vec<String>,
    resolved: bool,
    outdated: bool,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlResponse {
    data: Option<GitHubGraphQlData>,
    #[serde(default)]
    errors: Vec<GitHubGraphQlError>,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlData {
    repository: Option<GitHubGraphQlRepository>,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GitHubGraphQlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: GitHubGraphQlThreadConnection,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlThreadConnection {
    nodes: Vec<GitHubGraphQlThread>,
    #[serde(rename = "pageInfo")]
    page_info: GitHubGraphQlPageInfo,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlThread {
    id: String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "isOutdated")]
    is_outdated: bool,
    comments: GitHubGraphQlCommentConnection,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlCommentConnection {
    nodes: Vec<GitHubGraphQlComment>,
}

#[derive(Debug, Deserialize)]
struct GitHubGraphQlComment {
    id: String,
    #[serde(rename = "databaseId")]
    database_id: Option<u64>,
}

async fn fetch_github_review_thread_states(
    repository: &str,
    pull_request: u64,
    token: &str,
) -> Result<Vec<GitHubReviewThreadState>, GitHubPublisherError> {
    let (owner, name) = repository.split_once('/').ok_or_else(|| {
        GitHubPublisherError::InvalidOption("repository must be owner/repo".to_string())
    })?;
    let client = reqwest::Client::new();
    let mut states = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100_u16 {
        let response = client
            .post("https://api.github.com/graphql")
            .bearer_auth(token)
            .header(reqwest::header::USER_AGENT, "bcode-github-review-importer")
            .json(&serde_json::json!({
                "query": "query($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100,after:$cursor){nodes{id isResolved isOutdated comments(first:100){nodes{id databaseId}}} pageInfo{hasNextPage endCursor}}}}}",
                "variables": {
                    "owner": owner,
                    "name": name,
                    "number": pull_request,
                    "cursor": cursor,
                }
            }))
            .send()
            .await?;
        let http_status = response.status();
        let body = response.text().await?;
        if !http_status.is_success() {
            return Err(GitHubPublisherError::Api(format!(
                "GraphQL status {http_status}: {}",
                body.trim()
            )));
        }
        let response: GitHubGraphQlResponse = serde_json::from_str(&body)?;
        if !response.errors.is_empty() {
            return Err(GitHubPublisherError::Api(
                response
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let connection = response
            .data
            .and_then(|data| data.repository)
            .and_then(|repository| repository.pull_request)
            .ok_or(GitHubPublisherError::MissingPullRequest)?
            .review_threads;
        states.extend(connection.nodes.into_iter().map(|thread| {
            GitHubReviewThreadState {
                thread_id: thread.id,
                comment_ids: thread
                    .comments
                    .nodes
                    .iter()
                    .filter_map(|comment| comment.database_id)
                    .collect(),
                comment_node_ids: thread
                    .comments
                    .nodes
                    .into_iter()
                    .map(|comment| comment.id)
                    .collect(),
                resolved: thread.is_resolved,
                outdated: thread.is_outdated,
            }
        }));
        if !connection.page_info.has_next_page {
            return Ok(states);
        }
        cursor = connection.page_info.end_cursor;
        if cursor.is_none() {
            return Err(GitHubPublisherError::Api(
                "GitHub review thread pagination omitted an end cursor".to_string(),
            ));
        }
    }
    Err(GitHubPublisherError::ImportLimitExceeded)
}

async fn fetch_github_review_comments(
    repository: &str,
    pull_request: u64,
    token: &str,
) -> Result<Vec<GitHubReviewCommentResponse>, GitHubPublisherError> {
    let client = reqwest::Client::new();
    let mut comments = Vec::new();
    for page in 1..=100_u16 {
        let url = format!(
            "https://api.github.com/repos/{repository}/pulls/{pull_request}/comments?per_page=100&page={page}"
        );
        let response = client
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "bcode-github-review-importer")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(GitHubPublisherError::Api(format!(
                "status {status}: {}",
                body.trim()
            )));
        }
        let mut page_comments: Vec<GitHubReviewCommentResponse> =
            serde_json::from_str(&body).map_err(GitHubPublisherError::Json)?;
        let exhausted = page_comments.len() < 100;
        comments.append(&mut page_comments);
        if exhausted {
            return Ok(comments);
        }
    }
    Err(GitHubPublisherError::ImportLimitExceeded)
}

fn map_github_review_comments(
    request: &ExternalReviewImportRequest,
    comments: Vec<GitHubReviewCommentResponse>,
    thread_states: &[GitHubReviewThreadState],
) -> ExternalReviewImportResponse {
    let mut grouped = std::collections::BTreeMap::<u64, Vec<GitHubReviewCommentResponse>>::new();
    for comment in comments {
        grouped
            .entry(comment.in_reply_to_id.unwrap_or(comment.id))
            .or_default()
            .push(comment);
    }
    let mut warnings = Vec::new();
    let threads = grouped
        .into_iter()
        .filter_map(|(root_id, mut comments)| {
            comments.sort_by_key(|comment| comment.id);
            let root = comments
                .iter()
                .find(|comment| comment.id == root_id)
                .or_else(|| comments.first())?;
            let mut mapping = map_external_anchor(request, root);
            let provider_state = thread_states.iter().find(|state| {
                state.comment_ids.contains(&root_id)
                    || state.comment_node_ids.contains(&root.node_id)
            });
            if provider_state.is_some_and(|state| state.outdated)
                && mapping.status == ExternalAnchorMappingStatus::Exact
            {
                mapping.status = ExternalAnchorMappingStatus::Approximate;
                mapping.message = Some(
                    "GitHub marks this thread outdated; the anchor matches historical coordinates"
                        .to_string(),
                );
            }
            if mapping.status == ExternalAnchorMappingStatus::Unmappable {
                warnings.push(format!(
                    "GitHub thread {root_id} at {} could not be mapped to the current review",
                    root.path
                ));
            }
            let state = if provider_state.is_some_and(|state| state.outdated) || root.line.is_none()
            {
                ExternalReviewThreadState::Outdated
            } else if provider_state.is_some_and(|state| state.resolved) {
                ExternalReviewThreadState::Resolved
            } else {
                ExternalReviewThreadState::Open
            };
            let identity = provider_state.map_or_else(
                || github_identity("thread", root_id, root.html_url.clone()),
                |state| ExternalReviewIdentity {
                    provider_id: "github".to_string(),
                    external_id: format!("thread:{}", state.thread_id),
                    url: root.html_url.clone(),
                },
            );
            Some(ExternalReviewThread {
                identity,
                state,
                mapping,
                comments: comments
                    .into_iter()
                    .map(|comment| ExternalReviewComment {
                        identity: github_identity("comment", comment.id, comment.html_url),
                        author: Some(github_identity(
                            "user",
                            comment.user.id,
                            comment.user.html_url,
                        )),
                        author_label: Some(comment.user.login),
                        body: comment.body,
                        created_at_ms: None,
                        updated_at_ms: None,
                    })
                    .collect(),
            })
        })
        .collect();
    ExternalReviewImportResponse {
        importer_id: PUBLISHER_ID.to_string(),
        threads,
        warnings,
    }
}

fn github_identity(kind: &str, id: u64, url: Option<String>) -> ExternalReviewIdentity {
    ExternalReviewIdentity {
        provider_id: "github".to_string(),
        external_id: format!("{kind}:{id}"),
        url,
    }
}

fn map_external_anchor(
    request: &ExternalReviewImportRequest,
    comment: &GitHubReviewCommentResponse,
) -> ExternalReviewAnchorMapping {
    let line = comment.line.or(comment.original_line);
    let start_line = comment.start_line.or(comment.original_start_line).or(line);
    let Some(file) = request
        .files
        .iter()
        .find(|file| file.display_path() == comment.path)
    else {
        return ExternalReviewAnchorMapping {
            status: ExternalAnchorMappingStatus::Unmappable,
            anchor: None,
            message: Some("file is not present in the current review".to_string()),
        };
    };
    let Some(line) = line else {
        return ExternalReviewAnchorMapping {
            status: ExternalAnchorMappingStatus::Unmappable,
            anchor: None,
            message: Some("GitHub marks this diff position as outdated".to_string()),
        };
    };
    let side = comment.side.as_deref().unwrap_or("RIGHT");
    let start_side = comment.start_side.as_deref().unwrap_or(side);
    let matching_lines = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|candidate| {
            if side == "LEFT" {
                candidate.old_line == Some(line)
            } else {
                candidate.new_line == Some(line)
            }
        })
        .collect::<Vec<_>>();
    if matching_lines.len() != 1 {
        return ExternalReviewAnchorMapping {
            status: ExternalAnchorMappingStatus::Unmappable,
            anchor: None,
            message: Some("diff line is absent or ambiguous in the current review".to_string()),
        };
    }
    let line_kind = matching_lines[0].kind;
    let (old_line, new_line) = if side == "LEFT" {
        (Some(line), None)
    } else {
        (None, Some(line))
    };
    let (old_start, new_start) = if start_side == "LEFT" {
        (start_line, None)
    } else {
        (None, start_line)
    };
    ExternalReviewAnchorMapping {
        status: ExternalAnchorMappingStatus::Exact,
        anchor: Some(bcode_code_review_models::DraftAnchor {
            kind: bcode_code_review_models::ReviewAnchorKind::Range,
            file_path: comment.path.clone(),
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start,
            old_end: old_line,
            new_start,
            new_end: new_line,
            old_line,
            new_line,
            line_kind,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
        }),
        message: None,
    }
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
    #[error("missing GitHub token; set token_env/GITHUB_TOKEN/GH_TOKEN or run gh auth login")]
    MissingToken,
    #[error("invalid option: {0}")]
    InvalidOption(String),
    #[error("command failed to start: {0}")]
    Command(#[from] std::io::Error),
    #[error("command {program} failed: {stderr}")]
    CommandFailed { program: String, stderr: String },
    #[error("unmappable comments: {0:?}")]
    UnmappableComments(Vec<String>),
    #[error("GitHub import exceeded the bounded 10,000-comment discovery limit")]
    ImportLimitExceeded,
    #[error("GitHub API error: {0}")]
    Api(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("serialization failed: {0}")]
    Json(#[from] serde_json::Error),
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
        DraftAnchor, DraftComment, ReviewAnchorKind, ReviewTarget, ReviewThreadKind,
        ReviewThreadSeverity,
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
        assert_eq!(options.token_env, DEFAULT_TOKEN_ENV);
        assert_eq!(options.submit_event, DEFAULT_SUBMIT_EVENT);
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
    fn imported_comment_maps_exactly_to_current_diff_line() {
        let request = import_request(vec![bcode_code_review_models::ReviewLine {
            kind: ReviewLineKind::Added,
            old_line: None,
            new_line: Some(42),
            content: "added".to_string(),
        }]);
        let response = map_github_review_comments(
            &request,
            vec![github_import_comment(1, None, Some(42))],
            &[],
        );

        assert!(response.warnings.is_empty());
        let thread = response.threads.first().expect("imported thread");
        assert_eq!(thread.state, ExternalReviewThreadState::Open);
        assert_eq!(thread.mapping.status, ExternalAnchorMappingStatus::Exact);
        assert_eq!(
            thread
                .mapping
                .anchor
                .as_ref()
                .and_then(|anchor| anchor.new_line),
            Some(42)
        );
    }

    #[test]
    fn provider_thread_state_restores_resolved_and_graphql_identity() {
        let request = import_request(vec![bcode_code_review_models::ReviewLine {
            kind: ReviewLineKind::Added,
            old_line: None,
            new_line: Some(42),
            content: "added".to_string(),
        }]);
        let response = map_github_review_comments(
            &request,
            vec![github_import_comment(1, None, Some(42))],
            &[GitHubReviewThreadState {
                thread_id: "PRRT_graphql".to_string(),
                comment_ids: vec![1],
                comment_node_ids: vec!["PRRC_1".to_string()],
                resolved: true,
                outdated: false,
            }],
        );

        let thread = response.threads.first().expect("thread");
        assert_eq!(thread.state, ExternalReviewThreadState::Resolved);
        assert_eq!(thread.identity.external_id, "thread:PRRT_graphql");
    }

    #[test]
    fn provider_outdated_state_takes_precedence_over_resolved() {
        let request = import_request(vec![bcode_code_review_models::ReviewLine {
            kind: ReviewLineKind::Added,
            old_line: None,
            new_line: Some(42),
            content: "added".to_string(),
        }]);
        let response = map_github_review_comments(
            &request,
            vec![github_import_comment(1, None, Some(42))],
            &[GitHubReviewThreadState {
                thread_id: "PRRT_graphql".to_string(),
                comment_ids: vec![1],
                comment_node_ids: vec!["PRRC_1".to_string()],
                resolved: true,
                outdated: true,
            }],
        );

        assert_eq!(
            response.threads.first().expect("thread").state,
            ExternalReviewThreadState::Outdated
        );
        assert_eq!(
            response.threads.first().expect("thread").mapping.status,
            ExternalAnchorMappingStatus::Approximate
        );
    }

    #[test]
    fn outdated_import_is_preserved_as_unmappable_without_losing_replies() {
        let request = import_request(Vec::new());
        let response = map_github_review_comments(
            &request,
            vec![
                github_import_comment(1, None, None),
                github_import_comment(2, Some(1), None),
            ],
            &[],
        );

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

    fn import_request(
        lines: Vec<bcode_code_review_models::ReviewLine>,
    ) -> ExternalReviewImportRequest {
        ExternalReviewImportRequest {
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

    fn github_import_comment(
        id: u64,
        in_reply_to_id: Option<u64>,
        line: Option<u32>,
    ) -> GitHubReviewCommentResponse {
        GitHubReviewCommentResponse {
            id,
            node_id: format!("PRRC_{id}"),
            body: format!("comment {id}"),
            path: "src/lib.rs".to_string(),
            line,
            original_line: line,
            side: Some("RIGHT".to_string()),
            start_line: line,
            original_start_line: line,
            start_side: Some("RIGHT".to_string()),
            in_reply_to_id,
            html_url: Some(format!("https://github.test/comment/{id}")),
            user: GitHubReviewUser {
                id: 7,
                login: "reviewer".to_string(),
                html_url: Some("https://github.test/reviewer".to_string()),
            },
        }
    }

    #[test]
    fn maps_added_line_to_right_side_comment() {
        let thread = review_thread(bundle_line(
            "src/lib.rs",
            ReviewLineKind::Added,
            None,
            Some(42),
            1,
            "added",
        ));

        let comment = github_comment_for_thread(&thread).expect("comment");

        assert_eq!(comment.path, "src/lib.rs");
        assert_eq!(comment.line, 42);
        assert_eq!(comment.side, "RIGHT");
    }

    #[test]
    fn preview_is_local_and_does_not_require_submit_auth_or_http() {
        let request = ExternalPublishReviewRequest {
            bundle: ReviewBundle {
                review_id: "review".to_string(),
                title: "Review".to_string(),
                repo_root: PathBuf::from("/repo"),
                target: ReviewTarget::WorkingTreeUnstaged,
                surfaces: Vec::new(),
                files: Vec::new(),
                threads: vec![review_thread(bundle_line(
                    "src/lib.rs",
                    ReviewLineKind::Added,
                    None,
                    Some(42),
                    1,
                    "added",
                ))],
                generated_at_ms: 1,
            },
            options: serde_json::json!({
                "repository": "owner/repo",
                "pull_request": "123",
                "token_env": "INTENTIONALLY_MISSING_GITHUB_TOKEN"
            }),
        };

        let response = preview_for_request(&request).expect("offline preview");

        assert_eq!(response.publisher_id, PUBLISHER_ID);
        assert!(response.preview.contains("GitHub PR review preview"));
        assert!(response.preview.contains("owner/repo"));
        assert!(response.preview.contains("src/lib.rs"));
    }

    #[test]
    fn preview_reports_unmappable_comments() {
        let bundle = ReviewBundle {
            review_id: "review".to_string(),
            title: "Review".to_string(),
            repo_root: PathBuf::from("/repo"),
            target: ReviewTarget::WorkingTreeUnstaged,
            surfaces: Vec::new(),
            files: Vec::new(),
            threads: vec![review_thread(bundle_line(
                "src/lib.rs",
                ReviewLineKind::Context,
                None,
                None,
                0,
                "@@",
            ))],
            generated_at_ms: 1,
        };
        let request = ExternalPublishReviewRequest {
            bundle,
            options: serde_json::json!({
                "repository": "owner/repo",
                "pull_request": "123"
            }),
        };

        let response = preview_for_request(&request).expect("preview");

        assert!(response.preview.contains("Warnings"));
    }

    #[test]
    fn resolved_threads_are_not_published() {
        let mut thread = review_thread(bundle_line(
            "src/lib.rs",
            ReviewLineKind::Added,
            None,
            Some(42),
            1,
            "added",
        ));
        thread.resolved_at_ms = Some(123);
        let bundle = ReviewBundle {
            review_id: "review".to_string(),
            title: "Review".to_string(),
            repo_root: PathBuf::from("/repo"),
            target: ReviewTarget::WorkingTreeUnstaged,
            surfaces: Vec::new(),
            files: Vec::new(),
            threads: vec![thread],
            generated_at_ms: 1,
        };
        let options = GitHubPublishOptions::from_json(
            &serde_json::json!({
                "repository": "owner/repo",
                "pull_request": "123"
            }),
            &bundle,
        )
        .expect("options");

        let draft = github_review_draft(&bundle, &options);

        assert!(draft.comments.is_empty());
        assert!(
            draft
                .warnings
                .iter()
                .any(|warning| warning.contains("skipped 1 resolved"))
        );
    }

    #[test]
    fn maps_range_to_github_start_line() {
        let thread = review_thread_with_lines(vec![
            bundle_line(
                "src/lib.rs",
                ReviewLineKind::Added,
                None,
                Some(40),
                1,
                "first",
            ),
            bundle_line(
                "src/lib.rs",
                ReviewLineKind::Added,
                None,
                Some(42),
                2,
                "last",
            ),
        ]);

        let comment = github_comment_for_thread(&thread).expect("comment");

        assert_eq!(comment.line, 42);
        assert_eq!(comment.start_line, Some(40));
        assert_eq!(comment.start_side.as_deref(), Some("RIGHT"));
    }

    #[test]
    fn serializes_github_payload_for_comment_event() {
        let payload = GitHubCreateReviewRequest {
            body: "summary".to_string(),
            event: "COMMENT".to_string(),
            comments: vec![GitHubReviewComment {
                path: "src/lib.rs".to_string(),
                body: "body".to_string(),
                line: 42,
                side: "RIGHT".to_string(),
                start_line: Some(40),
                start_side: Some("RIGHT".to_string()),
            }],
        };

        let value = serde_json::to_value(&payload).expect("payload json");

        assert_eq!(value["event"], "COMMENT");
        assert_eq!(value["comments"][0]["start_line"], 40);
        assert_eq!(value["comments"][0]["side"], "RIGHT");
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
        file_path: &str,
        kind: ReviewLineKind,
        old_line: Option<u32>,
        new_line: Option<u32>,
        diff_row: u64,
        content: &str,
    ) -> ReviewBundleLine {
        ReviewBundleLine {
            file_path: file_path.to_string(),
            kind,
            old_line,
            new_line,
            diff_row,
            content: content.to_string(),
            surface_id: None,
            source_id: None,
        }
    }

    fn review_thread(line: ReviewBundleLine) -> ReviewBundleThread {
        review_thread_with_lines(vec![line])
    }

    fn review_thread_with_lines(lines: Vec<ReviewBundleLine>) -> ReviewBundleThread {
        let line = lines.first().expect("at least one line").clone();
        ReviewBundleThread {
            thread_id: "thread".to_string(),
            anchor: DraftAnchor {
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
            },
            comments: vec![DraftComment {
                comment_id: "comment".to_string(),
                thread_id: "thread".to_string(),
                anchor: DraftAnchor {
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
                },
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
            selected_lines: lines,
            selected_diff_lines: Vec::new(),
            hunk_context: Vec::new(),
        }
    }
}
