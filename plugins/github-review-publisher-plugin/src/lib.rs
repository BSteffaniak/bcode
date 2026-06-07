#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! GitHub review publisher plugin for Bcode.

use bcode_code_review_models::{
    ExternalPublishReviewRequest, OP_REVIEW_PUBLISHER_MANIFEST, OP_REVIEW_PUBLISHER_PREVIEW,
    OP_REVIEW_PUBLISHER_SUBMIT, PublishReviewPreviewResponse, PublishReviewResponse,
    REVIEW_PUBLISHER_INTERFACE_ID, ReviewBundle, ReviewBundleLine, ReviewBundleThread,
    ReviewLineKind, ReviewPublisherCapabilities, ReviewPublisherManifest,
};
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::env;
use std::fmt::Write as _;
use thiserror::Error;

const PUBLISHER_ID: &str = "github_pr_review";
const DEFAULT_TOKEN_ENV: &str = "GITHUB_TOKEN";
const DEFAULT_SUBMIT_EVENT: &str = "COMMENT";

/// GitHub review publisher plugin.
#[derive(Default)]
pub struct GitHubReviewPublisherPlugin;

impl RustPlugin for GitHubReviewPublisherPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != REVIEW_PUBLISHER_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported review publisher interface",
            );
        }
        match context.request.operation.as_str() {
            OP_REVIEW_PUBLISHER_MANIFEST => json_response(&github_manifest()),
            OP_REVIEW_PUBLISHER_PREVIEW => preview(&context.request),
            OP_REVIEW_PUBLISHER_SUBMIT => submit(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported GitHub publisher operation",
            ),
        }
    }
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
                "repository": { "type": "string", "description": "GitHub repository, owner/repo" },
                "pull_request": { "type": "string", "description": "Pull request number" },
                "token_env": { "type": "string", "description": "GitHub token env var, default GITHUB_TOKEN" },
                "submit_event": { "type": "string", "description": "COMMENT, REQUEST_CHANGES, or APPROVE" },
                "summary": { "type": "string", "description": "Optional review summary body" },
                "fallback_unmapped_to_summary": { "type": "string", "description": "Set to true to include unmappable inline comments in the review summary instead of failing submit" }
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
    let options = GitHubPublishOptions::from_json(&request.options)?;
    let draft = github_review_draft(&request.bundle, &options);
    let mut output = String::new();
    output.push_str("# GitHub PR review preview\n\n");
    let _ = writeln!(output, "* Repository: `{}`", options.repository);
    let _ = writeln!(output, "* Pull request: `#{}`", options.pull_request);
    let _ = writeln!(output, "* Event: `{}`", options.submit_event);
    let _ = write!(output, "* Inline comments: `{}`\n\n", draft.comments.len());
    if let Some(summary) = &draft.body {
        output.push_str("## Summary\n\n");
        output.push_str(summary);
        output.push_str("\n\n");
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
    let options = GitHubPublishOptions::from_json(&request.options)?;
    let token = options.token()?;
    let mut draft = github_review_draft(&request.bundle, &options);
    if !draft.warnings.is_empty() && !options.fallback_unmapped_to_summary {
        return Err(GitHubPublisherError::UnmappableComments(draft.warnings));
    }
    if options.fallback_unmapped_to_summary && !draft.warnings.is_empty() {
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
            &options.repository,
            options.pull_request,
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
            options.pull_request
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
    for thread in &bundle.threads {
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
    GitHubReviewDraft {
        body: options.summary.clone(),
        comments,
        warnings,
    }
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
    if let Some(session_id) = &thread.session_id {
        let _ = write!(body, "\n\n_Bcode session: `{session_id}`_");
    }
    body
}

#[derive(Debug, Clone)]
struct GitHubPublishOptions {
    repository: String,
    pull_request: u64,
    token_env: String,
    submit_event: String,
    summary: Option<String>,
    fallback_unmapped_to_summary: bool,
}

impl GitHubPublishOptions {
    fn from_json(value: &serde_json::Value) -> Result<Self, GitHubPublisherError> {
        let repository = string_option(value, "repository")
            .ok_or(GitHubPublisherError::MissingOption("repository"))?;
        if !repository.contains('/') {
            return Err(GitHubPublisherError::InvalidOption(
                "repository must be owner/repo".to_string(),
            ));
        }
        let pull_request = string_option(value, "pull_request")
            .ok_or(GitHubPublisherError::MissingOption("pull_request"))?
            .parse::<u64>()
            .map_err(|error| GitHubPublisherError::InvalidOption(error.to_string()))?;
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
            token_env,
            submit_event,
            summary: string_option(value, "summary"),
            fallback_unmapped_to_summary: bool_option(value, "fallback_unmapped_to_summary"),
        })
    }

    fn token(&self) -> Result<String, GitHubPublisherError> {
        env::var(&self.token_env).map_err(|_| GitHubPublisherError::MissingToken {
            env_var: self.token_env.clone(),
        })
    }
}

fn string_option(value: &serde_json::Value, key: &'static str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn bool_option(value: &serde_json::Value, key: &'static str) -> bool {
    value.get(key).is_some_and(|value| {
        value.as_bool().unwrap_or_else(|| {
            value
                .as_str()
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        })
    })
}

#[derive(Debug)]
struct GitHubReviewDraft {
    body: Option<String>,
    comments: Vec<GitHubReviewComment>,
    warnings: Vec<String>,
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

#[derive(Debug, Error)]
enum GitHubPublisherError {
    #[error("missing required option: {0}")]
    MissingOption(&'static str),
    #[error("invalid option: {0}")]
    InvalidOption(String),
    #[error("missing GitHub token env var: {env_var}")]
    MissingToken { env_var: String },
    #[error("unmappable comments: {0:?}")]
    UnmappableComments(Vec<String>),
    #[error("GitHub API error: {0}")]
    Api(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("runtime error: {0}")]
    Runtime(#[from] std::io::Error),
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

bcode_plugin_sdk::export_plugin!(
    GitHubReviewPublisherPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{DraftAnchor, DraftComment, ReviewTarget};
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
        let options = GitHubPublishOptions::from_json(&serde_json::json!({
            "repository": "owner/repo",
            "pull_request": "123"
        }))
        .expect("parse options");

        assert_eq!(options.repository, "owner/repo");
        assert_eq!(options.pull_request, 123);
        assert_eq!(options.token_env, DEFAULT_TOKEN_ENV);
        assert_eq!(options.submit_event, DEFAULT_SUBMIT_EVENT);
    }

    #[test]
    fn maps_added_line_to_right_side_comment() {
        let thread = review_thread(ReviewBundleLine {
            file_path: "src/lib.rs".to_string(),
            kind: ReviewLineKind::Added,
            old_line: None,
            new_line: Some(42),
            diff_row: 1,
            content: "added".to_string(),
        });

        let comment = github_comment_for_thread(&thread).expect("comment");

        assert_eq!(comment.path, "src/lib.rs");
        assert_eq!(comment.line, 42);
        assert_eq!(comment.side, "RIGHT");
    }

    #[test]
    fn preview_reports_unmappable_comments() {
        let bundle = ReviewBundle {
            review_id: "review".to_string(),
            title: "Review".to_string(),
            repo_root: PathBuf::from("/repo"),
            target: ReviewTarget::WorkingTreeUnstaged,
            files: Vec::new(),
            threads: vec![review_thread(ReviewBundleLine {
                file_path: "src/lib.rs".to_string(),
                kind: ReviewLineKind::Context,
                old_line: None,
                new_line: None,
                diff_row: 0,
                content: "@@".to_string(),
            })],
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
    fn maps_range_to_github_start_line() {
        let thread = review_thread_with_lines(vec![
            ReviewBundleLine {
                file_path: "src/lib.rs".to_string(),
                kind: ReviewLineKind::Added,
                old_line: None,
                new_line: Some(40),
                diff_row: 1,
                content: "first".to_string(),
            },
            ReviewBundleLine {
                file_path: "src/lib.rs".to_string(),
                kind: ReviewLineKind::Added,
                old_line: None,
                new_line: Some(42),
                diff_row: 2,
                content: "last".to_string(),
            },
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
        let options = GitHubPublishOptions::from_json(&serde_json::json!({
            "repository": "owner/repo",
            "pull_request": "123",
            "fallback_unmapped_to_summary": "true"
        }))
        .expect("parse options");

        assert!(options.fallback_unmapped_to_summary);
    }

    fn review_thread(line: ReviewBundleLine) -> ReviewBundleThread {
        review_thread_with_lines(vec![line])
    }

    fn review_thread_with_lines(lines: Vec<ReviewBundleLine>) -> ReviewBundleThread {
        let line = lines.first().expect("at least one line").clone();
        ReviewBundleThread {
            thread_id: "thread".to_string(),
            anchor: DraftAnchor {
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
            },
            comments: vec![DraftComment {
                comment_id: "comment".to_string(),
                thread_id: "thread".to_string(),
                anchor: DraftAnchor {
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
                },
                body: "comment body".to_string(),
                created_at_ms: 1,
                updated_at_ms: 1,
                session_id: None,
            }],
            session_id: None,
            selected_lines: lines,
            selected_diff_lines: Vec::new(),
            hunk_context: Vec::new(),
        }
    }
}
