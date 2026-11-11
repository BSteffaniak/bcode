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
/// Environment variable selecting the review interface when `--ui` is not given.
pub const REVIEW_UI_ENV: &str = "BCODE_REVIEW_UI";

#[derive(Debug, Parser)]
#[command(name = "review", about = "Review repository changes")]
struct ReviewCli {
    /// Review interface for interactive review subcommands (default: v1, or the
    /// `BCODE_REVIEW_UI` environment variable). Review home stays on v1 until V2 has one.
    #[arg(long, value_enum, global = true)]
    ui: Option<ReviewUi>,
    #[command(subcommand)]
    command: Option<ReviewCommand>,
}

/// Review interface implementation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum ReviewUi {
    /// The current review interface.
    #[default]
    V1,
    /// The Code Review V2 interface (in development).
    V2,
}

impl ReviewUi {
    /// The interface chosen by `--ui`, else by `BCODE_REVIEW_UI` (`v1` or `v2`, any case), else
    /// v1. An unrecognized environment value selects v1.
    fn select(flag: Option<Self>, env: Option<&str>) -> Self {
        flag.or_else(|| {
            env.and_then(|value| <Self as ValueEnum>::from_str(value.trim(), true).ok())
        })
        .unwrap_or_default()
    }

    const fn surface_kind(self) -> &'static str {
        match self {
            Self::V1 => REVIEW_SURFACE_KIND,
            Self::V2 => crate::v2::tui::CODE_REVIEW_V2_SURFACE_KIND,
        }
    }
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
pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon,
        command: ReviewCli::command,
        invoke,
    }
}
fn requires_daemon(matches: &clap::ArgMatches) -> bool {
    matches.subcommand_name() == Some("publish-github")
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = ReviewCli::from_arg_matches(&matches).map_err(|e| e.to_string())?;
        let ui = ReviewUi::select(cli.ui, std::env::var(REVIEW_UI_ENV).ok().as_deref());
        run(cli.command, ui).await.map_err(|e| e.to_string())
    })
}
/// Resolve a `--repo` argument against the CLI process working directory.
///
/// Code review git and state operations run inside the daemon, which has its own unrelated working
/// directory, so a relative argument such as the `.` default must be resolved here.
fn resolve_repo_argument(repo: &Path) -> Result<PathBuf, CliError> {
    std::fs::canonicalize(repo).map_err(|error| {
        CliError::Review(format!(
            "repository path {} is unavailable: {error}",
            repo.display()
        ))
    })
}

fn surface_outcome(
    repo: PathBuf,
    target: Option<ReviewTarget>,
    surface_kind: &str,
) -> StaticCliOutcome {
    let mut options = BTreeMap::new();
    if let Some(target) = target {
        options.insert(
            "target".into(),
            serde_json::to_string(&target).expect("review target serializes"),
        );
    }
    StaticCliOutcome {
        host_action: Some(StaticCliHostAction::OpenTuiSurface {
            surface_kind: surface_kind.into(),
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

async fn run(command: Option<ReviewCommand>, ui: ReviewUi) -> Result<StaticCliOutcome, CliError> {
    let Some(command) = command else {
        // Review home is V1 until V2 gains its home route (spec phase 5).
        return Ok(surface_outcome(
            resolve_repo_argument(Path::new("."))?,
            None,
            REVIEW_SURFACE_KIND,
        ));
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
    Ok(surface_outcome(
        resolve_repo_argument(&repo)?,
        Some(target),
        ui.surface_kind(),
    ))
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
    let repo = resolve_repo_argument(&request.repo)?;
    let bundle_payload = serde_json::to_vec(&ReviewContextRequest {
        repo_path: repo.clone(),
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
        None => detect_github_repository(&repo)?,
    };
    let pull_request = match request.pr {
        Some(pull_request) => pull_request,
        None => detect_pull_request_number(&repo)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_review_commands_defer_daemon_startup() {
        let registration = registration();
        for args in [
            vec!["review"],
            vec!["review", "unstaged"],
            vec!["review", "staged", "--repo", "/tmp/review"],
            vec!["review", "all"],
            vec!["review", "last-commit"],
            vec!["review", "range", "main", "HEAD"],
            vec!["review", "range", "main", "HEAD", "--two-dot"],
            vec!["review", "repo"],
        ] {
            let matches = (registration.command)().try_get_matches_from(args).unwrap();
            assert!(!(registration.requires_daemon)(&matches));
        }
    }

    #[test]
    fn ui_flag_reaches_every_interactive_subcommand_and_overrides_the_environment() {
        let registration = registration();
        for args in [
            vec!["review", "unstaged", "--ui", "v2"],
            vec!["review", "--ui", "v2", "staged"],
            vec!["review", "all", "--ui", "v2"],
            vec!["review", "last-commit", "--ui", "v2"],
            vec!["review", "range", "main", "HEAD", "--ui", "v2"],
            vec!["review", "repo", "--ui", "v2"],
        ] {
            let matches = (registration.command)()
                .try_get_matches_from(args.clone())
                .unwrap();
            let cli = ReviewCli::from_arg_matches(&matches).unwrap();
            assert_eq!(
                ReviewUi::select(cli.ui, Some("v1")),
                ReviewUi::V2,
                "{args:?}"
            );
        }
        assert_eq!(ReviewUi::select(None, Some("V2")), ReviewUi::V2);
        assert_eq!(ReviewUi::select(None, Some(" v2 ")), ReviewUi::V2);
        assert_eq!(ReviewUi::select(None, Some("other")), ReviewUi::V1);
        assert_eq!(ReviewUi::select(None, None), ReviewUi::V1);
        assert_eq!(
            ReviewUi::select(Some(ReviewUi::V1), Some("v2")),
            ReviewUi::V1
        );
    }

    #[test]
    fn v2_opens_the_v2_surface_with_the_target_and_home_stays_v1() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let repo = std::env::temp_dir();
        let outcome = runtime
            .block_on(run(Some(ReviewCommand::Unstaged { repo }), ReviewUi::V2))
            .unwrap();
        let Some(StaticCliHostAction::OpenTuiSurface {
            surface_kind,
            options,
            ..
        }) = outcome.host_action
        else {
            panic!("expected a surface");
        };
        assert_eq!(surface_kind, crate::v2::tui::CODE_REVIEW_V2_SURFACE_KIND);
        assert_eq!(
            serde_json::from_str::<ReviewTarget>(&options["target"]).unwrap(),
            ReviewTarget::WorkingTreeUnstaged
        );
        let outcome = runtime.block_on(run(None, ReviewUi::V2));
        // Home resolves the process working directory; only its surface kind matters here.
        if let Ok(StaticCliOutcome {
            host_action: Some(StaticCliHostAction::OpenTuiSurface { surface_kind, .. }),
        }) = outcome
        {
            assert_eq!(surface_kind, REVIEW_SURFACE_KIND);
        }
    }

    #[test]
    fn github_preview_and_submit_require_daemon_before_invocation() {
        let registration = registration();
        for args in [
            vec!["review", "publish-github"],
            vec!["review", "publish-github", "--submit"],
            vec!["review", "publish-github", "--target", "staged"],
        ] {
            let matches = (registration.command)().try_get_matches_from(args).unwrap();
            assert!((registration.requires_daemon)(&matches));
        }
    }

    #[test]
    fn repo_argument_resolves_relative_default_to_absolute_path() {
        let resolved = resolve_repo_argument(Path::new(".")).expect("current directory resolves");
        assert!(
            resolved.is_absolute(),
            "relative repo argument should resolve to an absolute path, got {}",
            resolved.display()
        );
    }

    #[test]
    fn repo_argument_resolves_nested_relative_path_against_current_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nested = temp.path().join("nested");
        std::fs::create_dir(&nested).expect("create nested directory");
        let expected = std::fs::canonicalize(&nested).expect("canonical nested directory");

        let resolved = resolve_repo_argument(&nested).expect("nested directory resolves");

        assert_eq!(resolved, expected);
    }

    #[test]
    fn repo_argument_rejects_missing_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("does-not-exist");

        let error = resolve_repo_argument(&missing).expect_err("missing directory should fail");

        assert!(
            matches!(error, CliError::Review(_)),
            "expected a review error, got {error:?}"
        );
    }

    #[test]
    fn surface_outcome_forwards_resolved_repository_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = std::fs::canonicalize(temp.path()).expect("canonical tempdir");

        let outcome = surface_outcome(repo.clone(), Some(ReviewTarget::IndexStaged));

        let Some(StaticCliHostAction::OpenTuiSurface { repo_path, .. }) = outcome.host_action
        else {
            panic!("expected an OpenTuiSurface host action");
        };
        assert_eq!(repo_path, Some(repo));
    }
}
