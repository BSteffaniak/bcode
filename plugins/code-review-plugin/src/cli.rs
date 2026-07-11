//! Statically bundled code-review CLI contribution.

use bcode_client::BcodeClient;
use bcode_code_review_models::{
    CODE_REVIEW_SERVICE_INTERFACE_ID, ExternalPublishReviewRequest, OP_REVIEW_BUNDLE_GET,
    OP_REVIEW_PUBLISHER_PREVIEW, OP_REVIEW_PUBLISHER_SUBMIT, REVIEW_PUBLISHER_INTERFACE_ID,
    ReviewBundle, ReviewContextRequest, ReviewTarget,
};
use bcode_plugin_sdk::{
    StaticCliFuture, StaticCliHostAction, StaticCliOutcome, StaticCliRegistration,
};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const REVIEW_SURFACE_KIND: &str = "code-review";
#[derive(Debug, Parser)]
#[command(name = "review", about = "Review repository changes")]
struct ReviewCli {
    #[command(subcommand)]
    command: Option<ReviewCommand>,
}
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Client(#[from] bcode_client::ClientError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("plugin service error {code}: {message}")]
    PluginService { code: String, message: String },
    #[error("code review error: {0}")]
    Review(String),
}
pub(super) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: false,
        command: ReviewCli::command,
        invoke,
    }
}
fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = ReviewCli::from_arg_matches(&matches).map_err(|e| e.to_string())?;
        run(cli.command).await.map_err(|e| e.to_string())
    })
}
fn surface_outcome(repo: PathBuf, target: Option<ReviewTarget>) -> StaticCliOutcome {
    let mut options = BTreeMap::new();
    if let Some(target) = target {
        options.insert(
            "target".into(),
            serde_json::to_string(&target).expect("review target serializes"),
        );
    }
    StaticCliOutcome {
        host_action: Some(StaticCliHostAction::OpenTuiSurface {
            surface_kind: REVIEW_SURFACE_KIND.into(),
            repo_path: Some(repo),
            options,
        }),
    }
}

#[derive(Debug, Subcommand)]
enum ReviewCommand {
    /// Review unstaged working-tree changes.
    Unstaged {
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Review staged index changes.
    Staged {
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Review staged and unstaged changes together.
    All {
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Review the last commit.
    LastCommit {
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Review an explicit revision range.
    Range {
        /// Base revision.
        base: String,
        /// Head revision.
        head: String,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Use two-dot range semantics instead of merge-base semantics.
        #[arg(long)]
        two_dot: bool,
    },
    /// Browse repository files and comment anywhere.
    Repo {
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Publish a review to GitHub without opening the TUI.
    PublishGithub {
        /// GitHub repository in owner/repo form.
        #[arg(long)]
        github_repo: Option<String>,
        /// GitHub pull request number.
        #[arg(long)]
        pr: Option<u64>,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// GitHub token environment variable.
        #[arg(long, default_value = "GITHUB_TOKEN")]
        token_env: String,
        /// GitHub review event.
        #[arg(long, value_enum, default_value_t = GithubSubmitEvent::Comment)]
        submit_event: GithubSubmitEvent,
        /// Optional review summary body.
        #[arg(long)]
        summary: Option<String>,
        /// Include unmappable comments in summary instead of failing submit.
        #[arg(long)]
        fallback_unmapped_to_summary: bool,
        /// Submit the review. Defaults to preview-only.
        #[arg(long)]
        submit: bool,
        /// Target to publish.
        #[arg(long, value_enum, default_value_t = ReviewTargetArg::Unstaged)]
        target: ReviewTargetArg,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum GithubSubmitEvent {
    Comment,
    RequestChanges,
    Approve,
}

impl GithubSubmitEvent {
    const fn as_github_event(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::RequestChanges => "REQUEST_CHANGES",
            Self::Approve => "APPROVE",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ReviewTargetArg {
    Unstaged,
    Staged,
    All,
    LastCommit,
}

impl From<ReviewTargetArg> for ReviewTarget {
    fn from(value: ReviewTargetArg) -> Self {
        match value {
            ReviewTargetArg::Unstaged => Self::WorkingTreeUnstaged,
            ReviewTargetArg::Staged => Self::IndexStaged,
            ReviewTargetArg::All => Self::WorkingTreeAndIndex,
            ReviewTargetArg::LastCommit => Self::LastCommit,
        }
    }
}

async fn run(command: Option<ReviewCommand>) -> Result<StaticCliOutcome, CliError> {
    let Some(command) = command else {
        return Ok(surface_outcome(PathBuf::from("."), None));
    };
    let (repo, target) = match command {
        ReviewCommand::Unstaged { repo } => (repo, ReviewTarget::WorkingTreeUnstaged),
        ReviewCommand::Staged { repo } => (repo, ReviewTarget::IndexStaged),
        ReviewCommand::All { repo } => (repo, ReviewTarget::WorkingTreeAndIndex),
        ReviewCommand::LastCommit { repo } => (repo, ReviewTarget::LastCommit),
        ReviewCommand::Range {
            repo,
            base,
            head,
            two_dot,
        } => (
            repo,
            ReviewTarget::CommitRange {
                base,
                head,
                merge_base: !two_dot,
            },
        ),
        ReviewCommand::Repo { repo } => (repo, ReviewTarget::Repository),
        ReviewCommand::PublishGithub {
            github_repo,
            pr,
            repo,
            token_env,
            submit_event,
            summary,
            fallback_unmapped_to_summary,
            submit,
            target,
        } => {
            publish_github_review(GithubPublishCliRequest {
                github_repo,
                pr,
                repo,
                token_env,
                submit_event,
                summary,
                fallback_unmapped_to_summary,
                submit,
                target: target.into(),
            })
            .await?;
            return Ok(StaticCliOutcome::default());
        }
    };
    Ok(surface_outcome(repo, Some(target)))
}

struct GithubPublishCliRequest {
    github_repo: Option<String>,
    pr: Option<u64>,
    repo: PathBuf,
    token_env: String,
    submit_event: GithubSubmitEvent,
    summary: Option<String>,
    fallback_unmapped_to_summary: bool,
    submit: bool,
    target: ReviewTarget,
}

async fn publish_github_review(request: GithubPublishCliRequest) -> Result<(), CliError> {
    let client = BcodeClient::default_endpoint();
    let bundle_payload = serde_json::to_vec(&ReviewContextRequest {
        repo_path: request.repo.clone(),
        target: request.target,
    })?;
    let bundle_response = client
        .call_plugin_service(
            CODE_REVIEW_SERVICE_INTERFACE_ID.to_string(),
            OP_REVIEW_BUNDLE_GET.to_string(),
            bundle_payload,
        )
        .await?;
    let bundle = plugin_response_json::<ReviewBundle>(bundle_response)?;
    let repository = match request.github_repo {
        Some(repository) => repository,
        None => detect_github_repository(&request.repo)?,
    };
    let pull_request = match request.pr {
        Some(pull_request) => pull_request,
        None => detect_pull_request_number(&request.repo)?,
    };
    let mut options = serde_json::json!({
        "repository": repository,
        "pull_request": pull_request.to_string(),
        "token_env": request.token_env,
        "submit_event": request.submit_event.as_github_event(),
    });
    if let Some(summary) = request.summary {
        options["summary"] = serde_json::Value::String(summary);
    }
    if request.fallback_unmapped_to_summary {
        options["fallback_unmapped_to_summary"] = serde_json::Value::Bool(true);
    }
    let publish_payload = serde_json::to_vec(&ExternalPublishReviewRequest { bundle, options })?;
    let operation = if request.submit {
        OP_REVIEW_PUBLISHER_SUBMIT
    } else {
        OP_REVIEW_PUBLISHER_PREVIEW
    };
    let response = client
        .call_plugin_service(
            REVIEW_PUBLISHER_INTERFACE_ID.to_string(),
            operation.to_string(),
            publish_payload,
        )
        .await?;
    let value = plugin_response_json::<serde_json::Value>(response)?;
    if request.submit {
        println!("{}", value["message"].as_str().unwrap_or("submitted"));
        if let Some(output) = value["output"].as_str() {
            println!("{output}");
        }
    } else if let Some(preview) = value["preview"].as_str() {
        println!("{preview}");
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

fn plugin_response_json<T: for<'de> Deserialize<'de>>(
    response: bcode_ipc::PluginServiceResponse,
) -> Result<T, CliError> {
    if let Some(error) = response.error {
        return Err(CliError::PluginService {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_slice(&response.payload).map_err(CliError::Json)
}

fn detect_github_repository(repo: &Path) -> Result<String, CliError> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Err(CliError::Review(
            "failed to detect GitHub repository from origin remote; pass --github-repo".to_string(),
        ));
    }
    let remote = String::from_utf8_lossy(&output.stdout);
    parse_github_remote_url(remote.trim()).ok_or_else(|| {
        CliError::Review(
            "origin remote is not a GitHub owner/repo URL; pass --github-repo".to_string(),
        )
    })
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

fn detect_pull_request_number(repo: &Path) -> Result<u64, CliError> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Err(CliError::Review(
            "failed to detect current branch; pass --pr".to_string(),
        ));
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_pull_request_from_branch(&branch).ok_or_else(|| {
        CliError::Review("failed to detect pull request number from branch; pass --pr".to_string())
    })
}

fn parse_pull_request_from_branch(branch: &str) -> Option<u64> {
    branch
        .strip_prefix("pull/")
        .and_then(|rest| rest.split('/').next())
        .or_else(|| branch.strip_prefix("pr/"))
        .and_then(|value| value.parse().ok())
}
