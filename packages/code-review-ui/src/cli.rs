//! The `review` command line, shared by `bcode review` and the standalone `bcode-review`.
//!
//! * Without a subcommand the app opens on the review home of the current directory's
//!   repository; each review subcommand opens a transient review (or resolves a pull request,
//!   stack, or saved review first) in the app.
//! * `publish` previews or submits a review of a pull request to its review-source provider
//!   without opening the app, through the built-in providers in-process (no daemon).
//!   `publish-github` is an alias kept for existing scripts.

use crate::entry::Entry;
use bcode_code_review_models::{
    PublishReviewRequest, PullLocator, ReviewBundle, ReviewSourceKind,
    ReviewSourcePullsListRequest, ReviewSubmitRequest, ReviewTarget, ReviewVerdict,
};
use clap::{Parser, Subcommand, ValueEnum};
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
    /// The command needs a running Bcode, and none is running.
    #[error("Bcode is not running; start bcode and try again")]
    BcodeNotRunning,
    /// The installed Bcode could not be reached.
    #[error(transparent)]
    InstalledBcode(#[from] bcode_client::InstalledBcodeError),
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

/// Whether `command` needs the Bcode daemon before it runs. None does: the app reaches it only
/// for AI questions and plugin services, and `publish` uses the built-in providers.
#[must_use]
pub const fn requires_daemon(command: Option<&ReviewCommand>) -> bool {
    let _ = command;
    false
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
    /// Publish to a pull request without opening the app.
    Publish(Box<PublishCliRequest>),
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
        Err(request) => Ok(Invocation::Publish(request)),
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
        Invocation::Publish(request) => publish_review(*request).await,
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
    /// Preview (or, with `--submit`, submit) a review of a pull request on its provider,
    /// without opening the app.
    #[command(alias = "publish-github")]
    Publish {
        /// Review-source provider id; when absent, the one built-in provider that submits
        /// reviews.
        #[arg(long)]
        provider: Option<String>,
        /// Provider repository (detected from the remotes when absent).
        #[arg(long, alias = "github-repo")]
        repository: Option<String>,
        /// Pull request number (the current branch's pull request when absent).
        #[arg(long, alias = "pr")]
        pull: Option<u64>,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// What the review says.
        #[arg(long, value_enum, alias = "submit-event", default_value_t = VerdictArg::Comment)]
        verdict: VerdictArg,
        /// Optional review summary body.
        #[arg(long)]
        summary: Option<String>,
        /// Put comments that cannot be placed inline into the summary instead of failing.
        #[arg(long, alias = "fallback-unmapped-to-summary")]
        unplaced_into_summary: bool,
        /// Submit the review. Defaults to preview-only.
        #[arg(long)]
        submit: bool,
        /// Target to publish.
        #[arg(long, value_enum, default_value_t = ReviewTargetArg::Unstaged)]
        target: ReviewTargetArg,
        /// Accepted for older scripts and ignored: providers find their own credentials.
        #[arg(long, hide = true)]
        token_env: Option<String>,
    },
}

/// Verdict of `publish`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum VerdictArg {
    /// A comment-only review.
    Comment,
    /// Request changes.
    RequestChanges,
    /// Approve.
    Approve,
}

impl From<VerdictArg> for ReviewVerdict {
    fn from(value: VerdictArg) -> Self {
        match value {
            VerdictArg::Comment => Self::Comment,
            VerdictArg::RequestChanges => Self::RequestChanges,
            VerdictArg::Approve => Self::Approve,
        }
    }
}

/// Target of `publish`.
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

/// The entry an interactive subcommand opens, or the publish request of `publish`.
fn entry_of(command: ReviewCommand) -> Result<(PathBuf, Entry), Box<PublishCliRequest>> {
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
        ReviewCommand::Publish {
            provider,
            repository,
            pull,
            repo,
            verdict,
            summary,
            unplaced_into_summary,
            submit,
            target,
            token_env: _,
        } => {
            return Err(Box::new(PublishCliRequest {
                provider,
                repository,
                pull,
                repo,
                verdict: verdict.into(),
                summary,
                unplaced_into_summary,
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

/// A `publish` request.
#[derive(Debug)]
pub struct PublishCliRequest {
    provider: Option<String>,
    repository: Option<String>,
    pull: Option<u64>,
    repo: PathBuf,
    verdict: ReviewVerdict,
    summary: Option<String>,
    unplaced_into_summary: bool,
    submit: bool,
    target: ReviewTarget,
}

/// Build the review's bundle from the stored state, then preview or submit it with the pull
/// request's provider (a built-in one, in-process).
async fn publish_review(request: PublishCliRequest) -> Result<(), CliError> {
    let repo = resolve_repo_argument(&request.repo)?;
    let registry = crate::run::builtin_providers();
    let route = publishing_provider(&registry, request.provider.as_deref())?;
    let provider = std::sync::Arc::clone(&route.provider);
    let publisher_id = format!(
        "{}{}",
        crate::publish::PROVIDER_PUBLISHER_PREFIX,
        route.manifest.id
    );
    let bundle = review_bundle(&repo, request.target.clone(), publisher_id).await?;
    let submit = request.submit;
    let output = tokio::task::spawn_blocking(move || -> Result<String, CliError> {
        let failed = |error: bcode_code_review::provider::ProviderCallError| {
            CliError::Review(crate::conversation::error_text(&error.code, &error.message))
        };
        let number = match request.pull {
            Some(number) => number,
            None => current_pull(&*provider, &repo, request.repository.as_deref())?,
        };
        let review = ReviewSubmitRequest {
            pull: PullLocator {
                repo_path: repo,
                repository: request.repository,
                number,
            },
            verdict: request.verdict,
            body: request.summary.unwrap_or_default(),
            head_oid: None,
            bundle: Some(bundle),
            replies: Vec::new(),
            unplaced_into_body: request.unplaced_into_summary,
        };
        if submit {
            let response = provider.submit_review(&review).map_err(failed)?;
            Ok(response.url.map_or_else(
                || response.message.clone(),
                |url| format!("{}\n{url}", response.message),
            ))
        } else {
            Ok(provider.preview_review(&review).map_err(failed)?.preview)
        }
    })
    .await
    .map_err(|error| CliError::Review(format!("publishing did not finish: {error}")))??;
    println!("{output}");
    Ok(())
}

/// The provider `id` names, else the one provider that submits reviews.
fn publishing_provider<'a>(
    registry: &'a bcode_code_review::provider::ProviderRegistry,
    id: Option<&str>,
) -> Result<&'a bcode_code_review::provider::ProviderRoute, CliError> {
    let reviewing: Vec<&bcode_code_review::provider::ProviderRoute> = registry
        .sources()
        .iter()
        .filter(|route| route.manifest.conversation.verdicts.any())
        .collect();
    let names = || {
        reviewing
            .iter()
            .map(|route| route.manifest.id.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };
    id.map_or_else(
        || match reviewing.as_slice() {
            [only] => Ok(*only),
            [] => Err(CliError::Review(
                "no review-source provider submits reviews".to_owned(),
            )),
            _ => Err(CliError::Review(format!(
                "pass --provider (one of: {})",
                names()
            ))),
        },
        |id| {
            reviewing
                .iter()
                .copied()
                .find(|route| route.manifest.id == id)
                .ok_or_else(|| {
                    CliError::Review(format!(
                        "no provider {id} submits reviews (available: {})",
                        names()
                    ))
                })
        },
    )
}

/// The open pull request of the current branch, as `provider` finds it.
fn current_pull(
    provider: &dyn bcode_code_review::provider::ReviewSourceProvider,
    repo: &Path,
    repository: Option<&str>,
) -> Result<u64, CliError> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo)
        .output()?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || branch.is_empty() {
        return Err(CliError::Review(
            "no current branch to find the pull request of; pass --pull".to_owned(),
        ));
    }
    let listed = provider
        .list_pulls(&ReviewSourcePullsListRequest {
            repo_path: repo.to_path_buf(),
            repository: repository.map(str::to_owned),
            query: Some(branch.clone()),
        })
        .map_err(|error| {
            CliError::Review(crate::conversation::error_text(&error.code, &error.message))
        })?;
    listed
        .pulls
        .iter()
        .find(|pull| pull.head_branch == branch)
        .map(|pull| pull.number)
        .ok_or_else(|| CliError::Review(format!("no open pull request for {branch}; pass --pull")))
}

/// The provider-neutral bundle of `target`'s drafts in the repository at `repo`, read with the
/// repository's configured review state.
async fn review_bundle(
    repo: &Path,
    target: ReviewTarget,
    publisher_id: String,
) -> Result<ReviewBundle, CliError> {
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
        publisher_id,
        options: serde_json::Value::Null,
    };
    tokio::task::spawn_blocking(move || store.review_bundle(request))
        .await
        .map_err(|error| CliError::Review(format!("building the review bundle failed: {error}")))?
        .map_err(|error| CliError::Review(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

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
    fn publish_needs_no_daemon_and_publishes_instead_of_opening_the_review() {
        let command = parse(&[
            "review",
            "publish",
            "--provider",
            "forge",
            "--repository",
            "owner/repo",
            "--pull",
            "7",
            "--target",
            "last-commit",
            "--verdict",
            "request-changes",
            "--summary",
            "LGTM",
            "--unplaced-into-summary",
            "--submit",
        ])
        .unwrap();
        assert!(!requires_daemon(command.as_ref()));
        let Invocation::Publish(request) = invocation(command).expect("command resolves") else {
            panic!("publish must not open the app");
        };
        assert_eq!(request.provider.as_deref(), Some("forge"));
        assert_eq!(request.repository.as_deref(), Some("owner/repo"));
        assert_eq!(request.pull, Some(7));
        assert_eq!(request.target, ReviewTarget::LastCommit);
        assert_eq!(request.verdict, ReviewVerdict::RequestChanges);
        assert_eq!(request.summary.as_deref(), Some("LGTM"));
        assert!(request.unplaced_into_summary);
        assert!(request.submit);
    }

    #[test]
    fn the_old_publish_github_command_line_still_reads() {
        let command = parse(&[
            "review",
            "publish-github",
            "--github-repo",
            "owner/repo",
            "--pr",
            "7",
            "--token-env",
            "GITHUB_TOKEN",
            "--submit-event",
            "approve",
            "--fallback-unmapped-to-summary",
        ])
        .unwrap();
        let Invocation::Publish(request) = invocation(command).expect("command resolves") else {
            panic!("publish-github must not open the app");
        };
        assert_eq!(request.provider, None);
        assert_eq!(request.repository.as_deref(), Some("owner/repo"));
        assert_eq!(request.pull, Some(7));
        assert_eq!(request.verdict, ReviewVerdict::Approve);
        assert!(request.unplaced_into_summary);
        assert!(!request.submit);
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
