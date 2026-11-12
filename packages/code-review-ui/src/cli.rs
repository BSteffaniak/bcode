//! The `review` command line, shared by `bcode review` and the standalone `bcode-review`.
//!
//! * Without a subcommand the app opens on the review home of the current directory's
//!   repository; each review subcommand opens a transient review (or resolves a pull request,
//!   stack, or saved review first) in the app.
//! * `publish-github` previews or submits a review to a GitHub pull request without opening the
//!   app. It is the only subcommand that needs the Bcode daemon.

use crate::entry::Entry;
use bcode_client::BcodeClient;
use bcode_code_review_models::{
    ExternalPublishReviewRequest, OP_REVIEW_PUBLISHER_PREVIEW, OP_REVIEW_PUBLISHER_SUBMIT,
    PublishReviewRequest, REVIEW_PUBLISHER_INTERFACE_ID, ReviewBundle, ReviewSourceKind,
    ReviewTarget,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `review [SUBCOMMAND]`.
#[derive(Debug, Parser)]
#[command(name = "review", about = "Review repository changes")]
pub struct ReviewCli {
    /// What to open or do; the review home when absent.
    #[command(subcommand)]
    pub command: Option<ReviewCommand>,
}

/// Failure of a review command.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The Bcode daemon could not be reached.
    #[error(transparent)]
    Client(#[from] bcode_client::ClientError),
    /// The command needs a running Bcode, and none is running (the standalone app never starts
    /// one, because it is not the Bcode program).
    #[error("Bcode is not running; start bcode and try again")]
    BcodeNotRunning,
    /// A payload could not be encoded or decoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A file or process could not be used.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A plugin service answered with an error.
    #[error("plugin service error {code}: {message}")]
    PluginService {
        /// Service error code.
        code: String,
        /// Service error message.
        message: String,
    },
    /// The review could not be opened or published.
    #[error("code review error: {0}")]
    Review(String),
    /// The app failed.
    #[error(transparent)]
    App(#[from] crate::RunError),
}

/// Whether `command` needs the Bcode daemon before it runs (only `publish-github` does; the app
/// starts without one and reaches it only for AI questions and plugin services).
#[must_use]
pub const fn requires_daemon(command: Option<&ReviewCommand>) -> bool {
    matches!(command, Some(ReviewCommand::PublishGithub { .. }))
}

/// What a command line asks for.
#[derive(Debug)]
pub enum Invocation {
    /// Open the app for the repository at `repo` on `entry`.
    Open {
        /// Absolute repository path.
        repo: PathBuf,
        /// What the app opens first.
        entry: Entry,
    },
    /// Publish to GitHub without opening the app.
    PublishGithub(Box<GithubPublishCliRequest>),
}

/// Resolve `command` into what it asks for, with its repository path made absolute.
///
/// # Errors
///
/// * [`CliError::Review`] when the repository path does not exist.
pub fn invocation(command: Option<ReviewCommand>) -> Result<Invocation, CliError> {
    let Some(command) = command else {
        return Ok(Invocation::Open {
            repo: resolve_repo_argument(Path::new("."))?,
            entry: Entry::Home,
        });
    };
    match entry_of(command) {
        Ok((repo, entry)) => Ok(Invocation::Open {
            repo: resolve_repo_argument(&repo)?,
            entry,
        }),
        Err(request) => Ok(Invocation::PublishGithub(request)),
    }
}

/// Run `command`: open the app, or publish. `bcode_program` is the Bcode executable the app
/// runs inside, when it does (see [`crate::RunOptions::bcode_program`]).
///
/// # Errors
///
/// * [`CliError`] when the repository is unavailable, configuration disables the review, the
///   app fails, or publishing fails.
pub async fn run(
    command: Option<ReviewCommand>,
    bcode_program: Option<PathBuf>,
) -> Result<(), CliError> {
    match invocation(command)? {
        Invocation::Open { repo, entry } => {
            let config = crate::config::load(&repo);
            if config.plugin_disabled {
                return Err(CliError::Review(
                    "code review is disabled by configuration ([plugins] disabled)".to_owned(),
                ));
            }
            crate::run(crate::RunOptions {
                path: repo,
                config,
                host: None,
                entry,
                bcode_program,
            })
            .await?;
            Ok(())
        }
        Invocation::PublishGithub(request) => {
            publish_github_review(*request, bcode_program.is_some()).await
        }
    }
}

/// Resolve a `--repo` argument against the process working directory.
fn resolve_repo_argument(repo: &Path) -> Result<PathBuf, CliError> {
    std::fs::canonicalize(repo).map_err(|error| {
        CliError::Review(format!(
            "repository path {} is unavailable: {error}",
            repo.display()
        ))
    })
}

/// Review subcommands.
#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
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
    /// Review the current branch against its base (merge-base semantics).
    Branch {
        /// Base revision; detected from the upstream, else the default branch, when omitted.
        base: Option<String>,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Compare two branches or revisions (merge-base semantics).
    Compare {
        /// Base revision.
        base: String,
        /// Head revision.
        head: String,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Use two-dot semantics instead of merge-base semantics.
        #[arg(long)]
        two_dot: bool,
    },
    /// Review one or more commits.
    Commit {
        /// Commit revisions; one range when they form a first-parent chain.
        #[arg(required = true, num_args = 1..)]
        revs: Vec<String>,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Review a pull request by number or URL (requires a review-source provider).
    Pr {
        /// Pull request number or URL.
        reference: String,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Review a whole stack of pull requests; without a reference, the stack of the current
    /// branch's pull request (requires a review-source provider).
    Stack {
        /// Pull request number or URL of any layer.
        reference: Option<String>,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Resume a saved review; without an id, the most recently updated one.
    Resume {
        /// Review workspace id.
        id: Option<String>,
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

/// GitHub review event of `publish-github`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum GithubSubmitEvent {
    /// A comment-only review.
    Comment,
    /// Request changes.
    RequestChanges,
    /// Approve.
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

/// Target of `publish-github`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ReviewTargetArg {
    /// Unstaged working-tree changes.
    Unstaged,
    /// Staged index changes.
    Staged,
    /// Staged and unstaged changes.
    All,
    /// The last commit.
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

/// The entry an interactive subcommand opens, or the publish request of `publish-github`.
fn entry_of(command: ReviewCommand) -> Result<(PathBuf, Entry), Box<GithubPublishCliRequest>> {
    Ok(match command {
        ReviewCommand::Unstaged { repo } => (repo, sources(ReviewSourceKind::WorkingTreeUnstaged)),
        ReviewCommand::Staged { repo } => (repo, sources(ReviewSourceKind::IndexStaged)),
        ReviewCommand::All { repo } => (repo, sources(ReviewSourceKind::WorkingTreeAndIndex)),
        ReviewCommand::LastCommit { repo } => (repo, sources(ReviewSourceKind::LastCommit)),
        ReviewCommand::Range {
            repo,
            base,
            head,
            two_dot,
        } => (
            repo,
            sources(ReviewSourceKind::CommitRange {
                base,
                head,
                merge_base: !two_dot,
            }),
        ),
        ReviewCommand::Repo { repo } => (repo, sources(ReviewSourceKind::Repository)),
        ReviewCommand::Branch { base, repo } => {
            (repo, sources(ReviewSourceKind::CurrentBranch { base }))
        }
        ReviewCommand::Compare {
            base,
            head,
            repo,
            two_dot,
        } => (
            repo,
            sources(ReviewSourceKind::BranchCompare {
                base_branch: base,
                head_branch: head,
                merge_base: !two_dot,
            }),
        ),
        ReviewCommand::Commit { revs, repo } => {
            let source = match <[String; 1]>::try_from(revs) {
                Ok([rev]) => ReviewSourceKind::Commit { rev },
                Err(revs) => ReviewSourceKind::Commits { revs },
            };
            (repo, sources(source))
        }
        ReviewCommand::Pr { reference, repo } => (repo, Entry::PullRequest { reference }),
        ReviewCommand::Stack { reference, repo } => (repo, Entry::Stack { reference }),
        ReviewCommand::Resume { id, repo } => (repo, Entry::Resume { workspace_id: id }),
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
            return Err(Box::new(GithubPublishCliRequest {
                github_repo,
                pr,
                repo,
                token_env,
                submit_event,
                summary,
                fallback_unmapped_to_summary,
                submit,
                target: target.into(),
            }));
        }
    })
}

/// A transient review of one source.
fn sources(source: ReviewSourceKind) -> Entry {
    Entry::Sources {
        title: None,
        sources: vec![source],
    }
}

/// A `publish-github` request.
#[derive(Debug)]
pub struct GithubPublishCliRequest {
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

/// Build the review's bundle from the stored state, then preview or submit it through the
/// daemon's review publisher. Inside Bcode the daemon is started when needed; the standalone app
/// requires one to be running already.
async fn publish_github_review(
    request: GithubPublishCliRequest,
    inside_bcode: bool,
) -> Result<(), CliError> {
    let repo = resolve_repo_argument(&request.repo)?;
    let bundle = review_bundle(&repo, request.target.clone()).await?;
    let availability = if inside_bcode {
        bcode_client::DaemonAvailability::AutoStart
    } else {
        bcode_client::DaemonAvailability::RequireRunning
    };
    let client = BcodeClient::default_endpoint().with_daemon_availability(availability);
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
        .await
        .map_err(daemon_error)?;
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

/// The provider-neutral bundle of `target`'s drafts in the repository at `repo`, read with the
/// repository's configured review state.
async fn review_bundle(repo: &Path, target: ReviewTarget) -> Result<ReviewBundle, CliError> {
    let config = crate::config::load(repo);
    if config.plugin_disabled {
        return Err(CliError::Review(
            "code review is disabled by configuration ([plugins] disabled)".to_owned(),
        ));
    }
    let store = bcode_code_review::ReviewStore::new(config.store);
    let request = PublishReviewRequest {
        repo_path: repo.to_path_buf(),
        target,
        workspace: None,
        excluded_thread_anchors: Vec::new(),
        publisher_id: GITHUB_PUBLISHER_ID.to_owned(),
        options: serde_json::Value::Null,
    };
    tokio::task::spawn_blocking(move || store.review_bundle(request))
        .await
        .map_err(|error| CliError::Review(format!("building the review bundle failed: {error}")))?
        .map_err(|error| CliError::Review(error.to_string()))
}

/// Publisher id of the GitHub pull request review publisher.
const GITHUB_PUBLISHER_ID: &str = "github_pr_review";

fn daemon_error(error: bcode_client::ClientError) -> CliError {
    if error.is_daemon_unavailable() {
        CliError::BcodeNotRunning
    } else {
        CliError::Client(error)
    }
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
    use clap::{CommandFactory, FromArgMatches};

    #[test]
    fn missing_daemon_reports_that_bcode_is_not_running() {
        let missing = bcode_client::ClientError::Transport(bcode_ipc::IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::NotFound),
        ));
        let error = daemon_error(missing);
        assert!(matches!(error, CliError::BcodeNotRunning), "{error}");
        assert_eq!(
            error.to_string(),
            "Bcode is not running; start bcode and try again"
        );
        let other = bcode_client::ClientError::Transport(bcode_ipc::IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        ));
        assert!(matches!(daemon_error(other), CliError::Client(_)));
    }

    fn parse(args: &[&str]) -> Result<Option<ReviewCommand>, clap::Error> {
        let matches = ReviewCli::command().try_get_matches_from(args)?;
        Ok(ReviewCli::from_arg_matches(&matches)?.command)
    }

    #[test]
    fn interactive_review_commands_defer_daemon_startup() {
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
            let command = parse(&args).unwrap();
            assert!(!requires_daemon(command.as_ref()), "{args:?}");
        }
    }

    #[test]
    fn unknown_flags_are_rejected() {
        for args in [
            vec!["review", "--ui", "unified"],
            vec!["review", "unstaged", "--ui", "unified"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    /// Parse each command line and check the entry it opens (without a daemon).
    fn assert_entries(cases: Vec<(Vec<&str>, Entry)>) {
        for (args, expected) in cases {
            let command = parse(&args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
            assert!(!requires_daemon(command.as_ref()), "{args:?}");
            let Invocation::Open { repo, entry } = invocation(command).expect("command resolves")
            else {
                panic!("{args:?} must open the app");
            };
            assert!(repo.is_absolute(), "{args:?}");
            assert_eq!(entry, expected, "{args:?}");
        }
    }

    #[test]
    fn review_opens_home_and_existing_subcommands_open_transient_reviews() {
        let review = |source| Entry::Sources {
            title: None,
            sources: vec![source],
        };
        assert_entries(vec![
            (vec!["review"], Entry::Home),
            (
                vec!["review", "unstaged"],
                review(ReviewSourceKind::WorkingTreeUnstaged),
            ),
            (
                vec!["review", "staged"],
                review(ReviewSourceKind::IndexStaged),
            ),
            (
                vec!["review", "all"],
                review(ReviewSourceKind::WorkingTreeAndIndex),
            ),
            (
                vec!["review", "last-commit"],
                review(ReviewSourceKind::LastCommit),
            ),
            (
                vec!["review", "range", "main", "HEAD"],
                review(ReviewSourceKind::CommitRange {
                    base: "main".into(),
                    head: "HEAD".into(),
                    merge_base: true,
                }),
            ),
            (
                vec!["review", "range", "main", "HEAD", "--two-dot"],
                review(ReviewSourceKind::CommitRange {
                    base: "main".into(),
                    head: "HEAD".into(),
                    merge_base: false,
                }),
            ),
            (vec!["review", "repo"], review(ReviewSourceKind::Repository)),
        ]);
    }

    #[test]
    fn branch_compare_and_commit_subcommands_open_transient_reviews() {
        assert_entries(vec![
            (
                vec!["review", "branch"],
                Entry::Sources {
                    title: None,
                    sources: vec![ReviewSourceKind::CurrentBranch { base: None }],
                },
            ),
            (
                vec!["review", "branch", "origin/main"],
                Entry::Sources {
                    title: None,
                    sources: vec![ReviewSourceKind::CurrentBranch {
                        base: Some("origin/main".into()),
                    }],
                },
            ),
            (
                vec!["review", "compare", "main", "feature"],
                Entry::Sources {
                    title: None,
                    sources: vec![ReviewSourceKind::BranchCompare {
                        base_branch: "main".into(),
                        head_branch: "feature".into(),
                        merge_base: true,
                    }],
                },
            ),
            (
                vec!["review", "compare", "main", "feature", "--two-dot"],
                Entry::Sources {
                    title: None,
                    sources: vec![ReviewSourceKind::BranchCompare {
                        base_branch: "main".into(),
                        head_branch: "feature".into(),
                        merge_base: false,
                    }],
                },
            ),
            (
                vec!["review", "commit", "abc123"],
                Entry::Sources {
                    title: None,
                    sources: vec![ReviewSourceKind::Commit {
                        rev: "abc123".into(),
                    }],
                },
            ),
            (
                vec!["review", "commit", "a1", "b2", "c3"],
                Entry::Sources {
                    title: None,
                    sources: vec![ReviewSourceKind::Commits {
                        revs: vec!["a1".into(), "b2".into(), "c3".into()],
                    }],
                },
            ),
        ]);
        assert!(
            parse(&["review", "commit"]).is_err(),
            "commit needs a revision"
        );
    }

    #[test]
    fn pr_stack_and_resume_subcommands_open_resolving_entries() {
        assert_entries(vec![
            (
                vec!["review", "pr", "42"],
                Entry::PullRequest {
                    reference: "42".into(),
                },
            ),
            (
                vec!["review", "pr", "https://example.com/o/r/pull/42"],
                Entry::PullRequest {
                    reference: "https://example.com/o/r/pull/42".into(),
                },
            ),
            (vec!["review", "stack"], Entry::Stack { reference: None }),
            (
                vec!["review", "stack", "#7"],
                Entry::Stack {
                    reference: Some("#7".into()),
                },
            ),
            (
                vec!["review", "resume"],
                Entry::Resume { workspace_id: None },
            ),
            (
                vec!["review", "resume", "ws-1"],
                Entry::Resume {
                    workspace_id: Some("ws-1".into()),
                },
            ),
        ]);
    }

    #[test]
    fn github_preview_and_submit_require_daemon_before_invocation() {
        for args in [
            vec!["review", "publish-github"],
            vec!["review", "publish-github", "--submit"],
            vec!["review", "publish-github", "--target", "staged"],
        ] {
            let command = parse(&args).unwrap();
            assert!(requires_daemon(command.as_ref()), "{args:?}");
        }
    }

    #[test]
    fn publish_github_publishes_instead_of_opening_the_review() {
        let command = parse(&[
            "review",
            "publish-github",
            "--github-repo",
            "owner/repo",
            "--pr",
            "7",
            "--target",
            "last-commit",
            "--submit-event",
            "request-changes",
            "--summary",
            "LGTM",
            "--fallback-unmapped-to-summary",
            "--submit",
        ])
        .unwrap();
        let Invocation::PublishGithub(request) = invocation(command).expect("command resolves")
        else {
            panic!("publish-github must not open the app");
        };
        assert_eq!(request.github_repo.as_deref(), Some("owner/repo"));
        assert_eq!(request.pr, Some(7));
        assert_eq!(request.target, ReviewTarget::LastCommit);
        assert_eq!(request.submit_event.as_github_event(), "REQUEST_CHANGES");
        assert_eq!(request.summary.as_deref(), Some("LGTM"));
        assert!(request.fallback_unmapped_to_summary);
        assert!(request.submit);
        assert_eq!(request.token_env, "GITHUB_TOKEN");
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
    fn subcommands_open_the_resolved_repository_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = std::fs::canonicalize(temp.path()).expect("canonical tempdir");
        let path = repo.to_string_lossy().into_owned();
        let command = parse(&["review", "staged", "--repo", &path]).unwrap();
        let Invocation::Open { repo: opened, .. } = invocation(command).expect("resolves") else {
            panic!("staged opens the app");
        };
        assert_eq!(opened, repo);
    }
}
