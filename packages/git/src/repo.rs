//! Repository operations.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use bcode_git_models::{
    Branch, ChangedFile, Commit, CommitMessageSource, DiffFile, DiffLineId, GitCommandRecord,
    HeadState, RebaseTodoItem, ReflogEntry, Remote, RemoteBranch, RepoOperation, RepoPaths,
    ResetMode, StashEntry, StatusSnapshot, Submodule, Tag, Worktree,
};

use crate::diff::parse_unified_diff;
use crate::parse;
use crate::patch::{PatchDirection, build_patch};
use crate::{CommandLog, GitError, GitInvocation, GitRunner};

/// Which pair of trees a diff compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    /// Index to worktree (unstaged changes).
    Unstaged,
    /// `HEAD` to index (staged changes).
    Staged,
    /// An untracked file against an empty file.
    Untracked,
    /// A commit against its first parent.
    Commit(String),
    /// Stash entry against its base.
    Stash(String),
    /// Two revisions.
    Range {
        /// Old side.
        from: String,
        /// New side.
        to: String,
    },
}

/// Where a patch is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTarget {
    /// The index only.
    Index,
    /// The worktree only.
    Worktree,
}

/// Commit options.
// Each flag maps to an independent git command-line option.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// Full message; the first line is the summary.
    pub message: String,
    /// Amend `HEAD` instead of creating a commit.
    pub amend: bool,
    /// Skip pre-commit and commit-msg hooks.
    pub no_verify: bool,
    /// Allow a commit without changes.
    pub allow_empty: bool,
    /// Keep the existing message when amending (ignores `message`).
    pub no_edit: bool,
}

/// Push options.
// Each flag maps to an independent git command-line option.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushOptions {
    /// Remote to push to, defaulting to the upstream remote.
    pub remote: Option<String>,
    /// Refspec or branch, defaulting to the current branch's push target.
    pub refspec: Option<String>,
    /// Use `--force-with-lease`.
    pub force_with_lease: bool,
    /// Use `--force`.
    pub force: bool,
    /// Configure the pushed branch as upstream.
    pub set_upstream: bool,
    /// Push tags too.
    pub tags: bool,
}

/// Stash options.
// Each flag maps to an independent git command-line option.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StashOptions {
    /// Optional message.
    pub message: Option<String>,
    /// Include untracked files.
    pub include_untracked: bool,
    /// Stash only staged changes.
    pub staged: bool,
    /// Keep staged changes in the index.
    pub keep_index: bool,
    /// Limit to these paths.
    pub paths: Vec<String>,
}

/// Side to take when resolving a conflicted file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    /// Our version.
    Ours,
    /// Their version.
    Theirs,
}

/// Log query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    /// Revision or range to list; defaults to `HEAD`.
    pub revision: Option<String>,
    /// Include all refs.
    pub all: bool,
    /// Maximum commits.
    pub limit: usize,
    /// Commits to skip.
    pub skip: usize,
    /// Restrict to commits touching this path.
    pub path: Option<String>,
    /// Message, author or content filter (`--grep`).
    pub grep: Option<String>,
    /// Author filter.
    pub author: Option<String>,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            revision: None,
            all: false,
            limit: 300,
            skip: 0,
            path: None,
            grep: None,
            author: None,
        }
    }
}

/// A discovered repository worktree.
#[derive(Debug, Clone)]
pub struct Repository {
    runner: GitRunner,
    paths: RepoPaths,
}

impl Repository {
    /// Discover the repository containing `path`.
    ///
    /// # Errors
    /// * [`GitError::NotARepository`] when `path` is not inside a worktree.
    /// * [`GitError::Io`] when git cannot be run.
    pub fn discover(path: impl AsRef<Path>, log: CommandLog) -> Result<Self, GitError> {
        let path = path.as_ref();
        let probe = GitRunner::new(path, log.clone());
        let output = probe
            .read([
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
            ])
            .map_err(|error| match error {
                GitError::Command { .. } => GitError::NotARepository(path.to_owned()),
                other => other,
            })?;
        let mut lines = output.lines();
        let (Some(root), Some(git_dir), Some(common_dir)) =
            (lines.next(), lines.next(), lines.next())
        else {
            return Err(GitError::NotARepository(path.to_owned()));
        };
        let root = PathBuf::from(root);
        Ok(Self {
            runner: GitRunner::new(&root, log),
            paths: RepoPaths {
                root,
                git_dir: PathBuf::from(git_dir),
                common_dir: PathBuf::from(common_dir),
            },
        })
    }

    /// Repository paths.
    #[must_use]
    pub const fn paths(&self) -> &RepoPaths {
        &self.paths
    }

    /// Worktree root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.paths.root
    }

    /// Underlying runner.
    #[must_use]
    pub const fn runner(&self) -> &GitRunner {
        &self.runner
    }

    /// Commands executed so far.
    #[must_use]
    pub fn command_log(&self) -> Vec<GitCommandRecord> {
        self.runner.log().records()
    }

    // ----- reads -----

    /// Where `HEAD` points.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn head(&self) -> Result<HeadState, GitError> {
        let symbolic = self.runner.run(&GitInvocation::read([
            "symbolic-ref",
            "--quiet",
            "--short",
            "HEAD",
        ]));
        match symbolic {
            Ok(output) => {
                let name = output.stdout_lossy().trim().to_owned();
                let born = self
                    .runner
                    .run(&GitInvocation::read([
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "HEAD",
                    ]))
                    .is_ok();
                Ok(if born {
                    HeadState::Branch { name }
                } else {
                    HeadState::Unborn { name }
                })
            }
            Err(GitError::Command { .. }) => Ok(HeadState::Detached {
                oid: self.runner.read(["rev-parse", "HEAD"])?.trim().to_owned(),
            }),
            Err(error) => Err(error),
        }
    }

    /// Multi-step operation in progress, from marker files in the git directory.
    #[must_use]
    pub fn operation(&self) -> RepoOperation {
        let git_dir = &self.paths.git_dir;
        let exists = |name: &str| git_dir.join(name).exists();
        if exists("rebase-merge") {
            RepoOperation::Rebasing
        } else if exists("rebase-apply") {
            if exists("rebase-apply/applying") {
                RepoOperation::ApplyingMailbox
            } else {
                RepoOperation::Rebasing
            }
        } else if exists("MERGE_HEAD") {
            RepoOperation::Merging
        } else if exists("CHERRY_PICK_HEAD") {
            RepoOperation::CherryPicking
        } else if exists("REVERT_HEAD") {
            RepoOperation::Reverting
        } else if self.paths.common_dir.join("BISECT_LOG").exists() || exists("BISECT_START") {
            RepoOperation::Bisecting
        } else {
            RepoOperation::None
        }
    }

    /// Working tree status.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn status(&self) -> Result<StatusSnapshot, GitError> {
        let output = self.runner.read([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--find-renames",
        ])?;
        parse::parse_status(&output)
    }

    /// Diff for a target, optionally limited to paths.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn diff(
        &self,
        target: &DiffTarget,
        paths: &[&str],
        context: u32,
    ) -> Result<Vec<DiffFile>, GitError> {
        let mut args: Vec<OsString> = Vec::new();
        let context_arg = format!("-U{context}");
        match target {
            DiffTarget::Unstaged => {
                args.extend(["diff".into(), "--no-ext-diff".into(), context_arg.into()]);
            }
            DiffTarget::Staged => args.extend([
                "diff".into(),
                "--no-ext-diff".into(),
                "--cached".into(),
                "--find-renames".into(),
                context_arg.into(),
            ]),
            DiffTarget::Untracked => {
                let [path] = paths else {
                    return Err(GitError::Unsupported(
                        "untracked diffs require exactly one path".into(),
                    ));
                };
                let output = self.runner.run(&GitInvocation::read([
                    "diff",
                    "--no-ext-diff",
                    "--no-index",
                    &context_arg,
                    "--",
                    "/dev/null",
                    path,
                ]));
                // `--no-index` exits 1 when files differ.
                let text = match output {
                    Ok(output) => output.stdout_lossy(),
                    Err(GitError::Command {
                        code: Some(1),
                        stdout,
                        ..
                    }) => stdout + "\n",
                    Err(error) => return Err(error),
                };
                return parse_unified_diff(&text);
            }
            DiffTarget::Commit(oid) => args.extend([
                "show".into(),
                "--no-ext-diff".into(),
                "--format=".into(),
                "--find-renames".into(),
                "--first-parent".into(),
                "-m".into(),
                context_arg.into(),
                oid.into(),
            ]),
            DiffTarget::Stash(selector) => args.extend([
                "stash".into(),
                "show".into(),
                "-p".into(),
                "--no-ext-diff".into(),
                "--include-untracked".into(),
                context_arg.into(),
                selector.into(),
            ]),
            DiffTarget::Range { from, to } => args.extend([
                "diff".into(),
                "--no-ext-diff".into(),
                "--find-renames".into(),
                context_arg.into(),
                from.into(),
                to.into(),
            ]),
        }
        if !paths.is_empty() {
            args.push("--".into());
            args.extend(paths.iter().map(OsString::from));
        }
        let output = self.runner.read(args)?;
        parse_unified_diff(&output)
    }

    /// Paths changed by a commit (against its first parent), a stash, or a range.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    /// * [`GitError::Unsupported`] for worktree targets.
    pub fn changed_files(&self, target: &DiffTarget) -> Result<Vec<ChangedFile>, GitError> {
        let output = match target {
            DiffTarget::Commit(oid) => self.runner.read([
                "show",
                "--format=",
                "--name-status",
                "-z",
                "--find-renames",
                "--first-parent",
                "-m",
                oid,
            ])?,
            DiffTarget::Stash(selector) => self.runner.read([
                "stash",
                "show",
                "--name-status",
                "-z",
                "--include-untracked",
                selector,
            ])?,
            DiffTarget::Range { from, to } => {
                self.runner
                    .read(["diff", "--name-status", "-z", "--find-renames", from, to])?
            }
            DiffTarget::Staged => {
                self.runner
                    .read(["diff", "--cached", "--name-status", "-z", "--find-renames"])?
            }
            DiffTarget::Unstaged | DiffTarget::Untracked => {
                return Err(GitError::Unsupported(
                    "use status for worktree changes".into(),
                ));
            }
        };
        Ok(parse::parse_name_status(&output))
    }

    /// Commits reachable from `HEAD` but not its upstream (unpushed commits).
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be run.
    pub fn unpushed(&self) -> Result<BTreeSet<String>, GitError> {
        match self.runner.read(["rev-list", "@{upstream}..HEAD"]) {
            Ok(output) => Ok(output.lines().map(str::to_owned).collect()),
            Err(GitError::Command { .. }) => Ok(BTreeSet::new()),
            Err(error) => Err(error),
        }
    }

    /// The branch others merge into: the remote's default branch (`origin/HEAD`), else a
    /// local `main` or `master`. `None` when there is none.
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be run.
    pub fn base_branch(&self) -> Result<Option<String>, GitError> {
        match self.runner.read([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ]) {
            Ok(output) if !output.trim().is_empty() => return Ok(Some(output.trim().to_owned())),
            Ok(_) | Err(GitError::Command { .. }) => {}
            Err(error) => return Err(error),
        }
        for name in ["main", "master"] {
            let reference = format!("refs/heads/{name}");
            match self
                .runner
                .read(["rev-parse", "--verify", "--quiet", &reference])
            {
                Ok(_) => return Ok(Some(name.to_owned())),
                Err(GitError::Command { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// Local branches whose work is already in [`Self::base_branch`] (safe to delete). The
    /// base and its local counterpart are not included. Empty without a base branch.
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be run.
    pub fn merged_branches(&self) -> Result<BTreeSet<String>, GitError> {
        let Some(base) = self.base_branch()? else {
            return Ok(BTreeSet::new());
        };
        let local_base = base.split_once('/').map_or(base.as_str(), |(_, name)| name);
        let output = match self.runner.read([
            "for-each-ref",
            "--merged",
            &base,
            "--format=%(refname:short)",
            "refs/heads",
        ]) {
            Ok(output) => output,
            Err(GitError::Command { .. }) => return Ok(BTreeSet::new()),
            Err(error) => return Err(error),
        };
        Ok(output
            .lines()
            .filter(|name| !name.is_empty() && *name != base && *name != local_base)
            .map(str::to_owned)
            .collect())
    }

    /// Check out one path as it was in a revision.
    ///
    /// # Errors
    /// * [`GitError`] when the path does not exist in the revision.
    pub fn checkout_path(&self, revision: &str, path: &str) -> Result<(), GitError> {
        self.with_paths(&["checkout", revision], &[path])
    }

    /// Commit the staged changes into an older commit (`git commit --fixup` plus autosquash).
    ///
    /// # Errors
    /// * [`GitError`] when the rebase stops or git fails.
    pub fn amend_into(&self, oid: &str) -> Result<(), GitError> {
        self.create_fixup(oid)?;
        let parent = format!("{oid}^");
        self.rev_parse(&parent)
            .map_or_else(|_| self.autosquash("--root"), |base| self.autosquash(&base))
    }

    /// Commit history.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn log(&self, query: &LogQuery) -> Result<Vec<Commit>, GitError> {
        let mut args: Vec<String> = vec![
            "log".into(),
            parse::LOG_FORMAT.into(),
            format!("--max-count={}", query.limit),
            format!("--skip={}", query.skip),
            "--topo-order".into(),
        ];
        if let Some(grep) = &query.grep {
            args.push(format!("--grep={grep}"));
            args.push("--regexp-ignore-case".into());
        }
        if let Some(author) = &query.author {
            args.push(format!("--author={author}"));
        }
        if query.revision.is_none() && !query.all && !self.has_head() {
            return Ok(Vec::new());
        }
        if query.all {
            args.push("--all".into());
        } else {
            args.push(query.revision.clone().unwrap_or_else(|| "HEAD".into()));
        }
        args.push("--".into());
        if let Some(path) = &query.path {
            args.push(path.clone());
        }
        match self.runner.read(&args) {
            Ok(output) => parse::parse_log(&output),
            // An unborn branch has no history.
            Err(GitError::Command { stderr, .. })
                if stderr.contains("does not have any commits")
                    || stderr.contains("unknown revision") =>
            {
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        }
    }

    /// Full commit message.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn commit_message(&self, oid: &str) -> Result<String, GitError> {
        Ok(self
            .runner
            .read(["log", "-1", "--format=%B", oid])?
            .trim_end()
            .to_owned())
    }

    /// Commits reachable from `to` but not `from`.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn commits_between(&self, from: &str, to: &str) -> Result<Vec<Commit>, GitError> {
        self.log(&LogQuery {
            revision: Some(format!("{from}..{to}")),
            limit: 10_000,
            ..LogQuery::default()
        })
    }

    /// Local branches, most recently committed first.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn branches(&self) -> Result<Vec<Branch>, GitError> {
        let output = self.runner.read([
            "for-each-ref",
            "--sort=-committerdate",
            parse::BRANCH_FORMAT,
            "refs/heads",
        ])?;
        parse::parse_branches(&output, self.root())
    }

    /// Remote-tracking branches.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn remote_branches(&self) -> Result<Vec<RemoteBranch>, GitError> {
        let output = self.runner.read([
            "for-each-ref",
            "--sort=refname",
            parse::REMOTE_BRANCH_FORMAT,
            "refs/remotes",
        ])?;
        parse::parse_remote_branches(&output)
    }

    /// Configured remotes.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn remotes(&self) -> Result<Vec<Remote>, GitError> {
        Ok(parse::parse_remotes(&self.runner.read(["remote", "-v"])?))
    }

    /// Tags, newest first.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn tags(&self) -> Result<Vec<Tag>, GitError> {
        let output = self.runner.read([
            "for-each-ref",
            "--sort=-creatordate",
            parse::TAG_FORMAT,
            "refs/tags",
        ])?;
        parse::parse_tags(&output)
    }

    /// Stash entries.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn stashes(&self) -> Result<Vec<StashEntry>, GitError> {
        let output = self.runner.read(["stash", "list", parse::STASH_FORMAT])?;
        parse::parse_stashes(&output)
    }

    /// `HEAD` reflog, newest first.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or output is malformed.
    pub fn reflog(&self, limit: usize) -> Result<Vec<ReflogEntry>, GitError> {
        let limit = format!("--max-count={limit}");
        match self
            .runner
            .read(["log", "-g", parse::REFLOG_FORMAT, &limit, "HEAD", "--"])
        {
            Ok(output) => parse::parse_reflog(&output),
            Err(GitError::Command { .. }) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Worktrees of this repository.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn worktrees(&self) -> Result<Vec<Worktree>, GitError> {
        Ok(parse::parse_worktrees(&self.runner.read([
            "worktree",
            "list",
            "--porcelain",
            "-z",
        ])?))
    }

    /// Submodules of this repository.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn submodules(&self) -> Result<Vec<Submodule>, GitError> {
        if !self.root().join(".gitmodules").exists() {
            return Ok(Vec::new());
        }
        Ok(parse::parse_submodules(
            &self.runner.read(["submodule", "status"])?,
        ))
    }

    /// File content at a revision (`HEAD:path`), the index (`:path`), or `None` when absent.
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be run.
    pub fn show_file(&self, spec: &str) -> Result<Option<Vec<u8>>, GitError> {
        match self.runner.run(&GitInvocation::read(["show", spec])) {
            Ok(output) => Ok(Some(output.stdout)),
            Err(GitError::Command { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Resolve a revision to a full object id.
    ///
    /// # Errors
    /// * [`GitError`] when the revision does not exist.
    pub fn rev_parse(&self, revision: &str) -> Result<String, GitError> {
        Ok(self
            .runner
            .read(["rev-parse", "--verify", "--end-of-options", revision])?
            .trim()
            .to_owned())
    }

    /// Merge base of two revisions.
    ///
    /// # Errors
    /// * [`GitError`] when no merge base exists.
    pub fn merge_base(&self, left: &str, right: &str) -> Result<String, GitError> {
        Ok(self
            .runner
            .read(["merge-base", left, right])?
            .trim()
            .to_owned())
    }

    /// Read a configuration value.
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be run.
    pub fn config_get(&self, key: &str) -> Result<Option<String>, GitError> {
        match self.runner.read(["config", "--get", key]) {
            Ok(value) => Ok(Some(value.trim().to_owned())),
            Err(GitError::Command { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    // ----- index and worktree -----

    /// Stage paths (including deletions).
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn stage(&self, paths: &[&str]) -> Result<(), GitError> {
        self.with_paths(&["add", "--all"], paths)
    }

    /// Stage everything.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn stage_all(&self) -> Result<(), GitError> {
        self.runner.write(["add", "--all"]).map(drop)
    }

    /// Unstage paths, keeping worktree changes.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn unstage(&self, paths: &[&str]) -> Result<(), GitError> {
        if self.has_head() {
            self.with_paths(&["restore", "--staged"], paths)
        } else {
            self.with_paths(&["rm", "--cached", "--quiet", "-r"], paths)
        }
    }

    /// Unstage everything.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn unstage_all(&self) -> Result<(), GitError> {
        if self.has_head() {
            self.runner.write(["reset", "--quiet"]).map(drop)
        } else {
            self.runner
                .write(["rm", "--cached", "--quiet", "-r", "."])
                .map(drop)
        }
    }

    /// Mark untracked paths as intent-to-add so their lines can be staged individually.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn intent_to_add(&self, paths: &[&str]) -> Result<(), GitError> {
        self.with_paths(&["add", "--intent-to-add"], paths)
    }

    /// Discard all changes to paths: restore tracked paths from `HEAD` and delete untracked
    /// ones. Conflicted or staged-only paths are restored in both index and worktree.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or an untracked file cannot be removed.
    pub fn discard(&self, tracked: &[&str], untracked: &[&str]) -> Result<(), GitError> {
        if !tracked.is_empty() {
            if self.has_head() {
                self.with_paths(
                    &["restore", "--staged", "--worktree", "--source=HEAD"],
                    tracked,
                )?;
            } else {
                self.with_paths(&["rm", "--cached", "--quiet", "-r", "-f"], tracked)?;
            }
        }
        if !untracked.is_empty() {
            self.with_paths(&["clean", "-f", "-d", "-q"], untracked)?;
        }
        Ok(())
    }

    /// Discard only worktree changes, keeping staged content.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn discard_unstaged(&self, paths: &[&str]) -> Result<(), GitError> {
        self.with_paths(&["restore", "--worktree"], paths)
    }

    /// Apply a patch to the index or worktree.
    ///
    /// # Errors
    /// * [`GitError::Command`] when the patch does not apply cleanly.
    pub fn apply_patch(
        &self,
        patch: &str,
        target: ApplyTarget,
        reverse: bool,
    ) -> Result<(), GitError> {
        let mut args = vec!["apply", "--whitespace=nowarn"];
        if target == ApplyTarget::Index {
            args.push("--cached");
        }
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.runner
            .run(&GitInvocation::write(args).stdin(patch.as_bytes()))
            .map(drop)
    }

    /// Stage selected lines of an unstaged diff.
    ///
    /// # Errors
    /// * [`GitError`] when the patch cannot be built or applied.
    pub fn stage_lines(
        &self,
        file: &DiffFile,
        selection: &BTreeSet<DiffLineId>,
    ) -> Result<(), GitError> {
        self.apply_selection(
            file,
            selection,
            PatchDirection::Forward,
            ApplyTarget::Index,
            false,
        )
    }

    /// Unstage selected lines of a staged diff.
    ///
    /// # Errors
    /// * [`GitError`] when the patch cannot be built or applied.
    pub fn unstage_lines(
        &self,
        file: &DiffFile,
        selection: &BTreeSet<DiffLineId>,
    ) -> Result<(), GitError> {
        self.apply_selection(
            file,
            selection,
            PatchDirection::Reverse,
            ApplyTarget::Index,
            true,
        )
    }

    /// Discard selected lines of an unstaged diff from the worktree.
    ///
    /// # Errors
    /// * [`GitError`] when the patch cannot be built or applied.
    pub fn discard_lines(
        &self,
        file: &DiffFile,
        selection: &BTreeSet<DiffLineId>,
    ) -> Result<(), GitError> {
        self.apply_selection(
            file,
            selection,
            PatchDirection::Reverse,
            ApplyTarget::Worktree,
            true,
        )
    }

    fn apply_selection(
        &self,
        file: &DiffFile,
        selection: &BTreeSet<DiffLineId>,
        direction: PatchDirection,
        target: ApplyTarget,
        reverse: bool,
    ) -> Result<(), GitError> {
        build_patch(file, selection, direction)?
            .map_or(Ok(()), |patch| self.apply_patch(&patch, target, reverse))
    }

    /// Resolve a conflicted path by taking one side, then stage it.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn resolve_conflict(&self, path: &str, side: ConflictSide) -> Result<(), GitError> {
        let flag = match side {
            ConflictSide::Ours => "--ours",
            ConflictSide::Theirs => "--theirs",
        };
        self.with_paths(&["checkout", flag], &[path])?;
        self.stage(&[path])
    }

    /// Append a pattern to the repository `.gitignore`.
    ///
    /// # Errors
    /// * [`GitError::Io`] when the file cannot be written.
    pub fn ignore(&self, pattern: &str) -> Result<(), GitError> {
        Self::append_line(&self.root().join(".gitignore"), pattern)
    }

    /// Append a pattern to `.git/info/exclude`.
    ///
    /// # Errors
    /// * [`GitError::Io`] when the file cannot be written.
    pub fn exclude(&self, pattern: &str) -> Result<(), GitError> {
        let info = self.paths.common_dir.join("info");
        std::fs::create_dir_all(&info)?;
        Self::append_line(&info.join("exclude"), pattern)
    }

    fn append_line(path: &Path, line: &str) -> Result<(), GitError> {
        use std::io::Write as _;
        let existing = std::fs::read(path).unwrap_or_default();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        if !existing.is_empty() && !existing.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Delete every untracked file and directory (ignored files stay).
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn clean_untracked(&self) -> Result<(), GitError> {
        self.runner.write(["clean", "-f", "-d"]).map(drop)
    }

    // ----- commits -----

    /// Create or amend a commit.
    ///
    /// # Errors
    /// * [`GitError`] when hooks reject the commit or git fails.
    pub fn commit(&self, options: &CommitOptions) -> Result<(), GitError> {
        let mut args = vec!["commit"];
        if options.amend {
            args.push("--amend");
        }
        if options.no_verify {
            args.push("--no-verify");
        }
        if options.allow_empty {
            args.push("--allow-empty");
        }
        let invocation = if options.no_edit {
            args.push("--no-edit");
            GitInvocation::write(args)
        } else {
            // The message is final text: lines starting with `#` are content, not comments.
            args.extend(["--cleanup=whitespace", "--file=-"]);
            GitInvocation::write(args).stdin(options.message.as_bytes())
        };
        self.runner.run(&invocation).map(drop)
    }

    /// Prefill text from a file-backed commit message source, or `None` when it is absent or
    /// blank. [`CommitMessageSource::Env`] belongs to the caller and always yields `None`.
    ///
    /// Drafts keep `#` lines (they are content); git-generated files and templates have
    /// comment lines stripped as `git commit` would.
    ///
    /// # Errors
    /// * [`GitError`] when git fails or a file cannot be read.
    pub fn commit_message_from(
        &self,
        source: CommitMessageSource,
    ) -> Result<Option<String>, GitError> {
        let Some(path) = self.commit_message_path(source)? else {
            return Ok(None);
        };
        let Some(raw) = read_message_file(&path)? else {
            return Ok(None);
        };
        let text = self.clean_message(&raw, !source.is_draft())?;
        if text.is_empty() {
            return Ok(None);
        }
        if source == CommitMessageSource::EditMsg && self.has_head() {
            // After a successful commit the file still holds that commit's message.
            let head = self.clean_message(&self.commit_message("HEAD")?, true)?;
            if head == text {
                return Ok(None);
            }
        }
        Ok(Some(text))
    }

    /// File holding a commit message source, whether or not it exists yet; `None` for the
    /// environment and for an unset `commit.template`.
    ///
    /// # Errors
    /// * [`GitError`] when git fails reading `commit.template`.
    pub fn commit_message_path(
        &self,
        source: CommitMessageSource,
    ) -> Result<Option<PathBuf>, GitError> {
        let git_dir = &self.paths.git_dir;
        Ok(Some(match source {
            CommitMessageSource::Env => return Ok(None),
            CommitMessageSource::BcodeDraft => git_dir.join(BCODE_COMMIT_DRAFT),
            CommitMessageSource::LazygitDraft => git_dir.join(LAZYGIT_COMMIT_DRAFT),
            CommitMessageSource::Merge => git_dir.join("MERGE_MSG"),
            CommitMessageSource::Squash => git_dir.join("SQUASH_MSG"),
            CommitMessageSource::EditMsg => git_dir.join("COMMIT_EDITMSG"),
            CommitMessageSource::Template => match self.config_path("commit.template")? {
                // Relative templates resolve against the worktree, as for `git commit`.
                Some(template) => self.paths.root.join(template),
                None => return Ok(None),
            },
        }))
    }

    /// Store an unsent commit message as bcode's draft; blank text removes the draft.
    ///
    /// # Errors
    /// * [`GitError`] when the file cannot be written.
    pub fn save_commit_draft(&self, message: &str) -> Result<(), GitError> {
        let path = self.paths.git_dir.join(BCODE_COMMIT_DRAFT);
        if message.trim().is_empty() {
            return remove_if_present(&path);
        }
        // Write then rename, so readers never see a partial file.
        let partial = self
            .paths
            .git_dir
            .join(format!("{BCODE_COMMIT_DRAFT}.partial"));
        std::fs::write(&partial, message)?;
        std::fs::rename(&partial, &path)?;
        Ok(())
    }

    /// Remove bcode's draft and, when its message was used, lazygit's.
    ///
    /// # Errors
    /// * [`GitError`] when a file exists but cannot be removed.
    pub fn clear_commit_drafts(&self, bcode: bool, lazygit: bool) -> Result<(), GitError> {
        if bcode {
            remove_if_present(&self.paths.git_dir.join(BCODE_COMMIT_DRAFT))?;
        }
        if lazygit {
            remove_if_present(&self.paths.git_dir.join(LAZYGIT_COMMIT_DRAFT))?;
        }
        Ok(())
    }

    fn clean_message(&self, raw: &str, strip_comments: bool) -> Result<String, GitError> {
        let args: &[&str] = if strip_comments {
            &["stripspace", "--strip-comments"]
        } else {
            &["stripspace"]
        };
        let output = self
            .runner
            .run(&GitInvocation::read(args.iter().copied()).stdin(raw.as_bytes()))?;
        Ok(output.stdout_lossy().trim_end().to_owned())
    }

    fn config_path(&self, key: &str) -> Result<Option<String>, GitError> {
        match self.runner.read(["config", "--path", "--get", key]) {
            Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_owned())),
            Ok(_) | Err(GitError::Command { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Reword `HEAD` or an older commit on the current branch.
    ///
    /// # Errors
    /// * [`GitError`] when the rebase stops or git fails.
    pub fn reword(&self, oid: &str, message: &str) -> Result<(), GitError> {
        let head = self.rev_parse("HEAD")?;
        let target = self.rev_parse(oid)?;
        if head == target {
            return self.commit(&CommitOptions {
                message: message.to_owned(),
                amend: true,
                no_verify: true,
                allow_empty: true,
                no_edit: false,
            });
        }
        let message_file = self.paths.git_dir.join("BCODE_REWORD_MSG");
        std::fs::write(&message_file, message)?;
        let quoted = shell_quote(&message_file.to_string_lossy());
        let parent = format!("{target}^");
        let base = self.rev_parse(&parent).ok();
        let result = self.rewrite_todo(
            base.as_deref(),
            &[(
                target.clone(),
                vec![
                    format!("pick {target}"),
                    format!("exec git commit --amend --no-verify --allow-empty --cleanup=whitespace --file={quoted}"),
                ],
            )],
        );
        let _ = std::fs::remove_file(&message_file);
        result
    }

    /// Revert commits.
    ///
    /// # Errors
    /// * [`GitError`] when the revert conflicts or git fails.
    pub fn revert(&self, oids: &[&str]) -> Result<(), GitError> {
        let mut args = vec!["revert", "--no-edit"];
        if oids.iter().any(|oid| self.is_merge(oid)) {
            args.extend(["-m", "1"]);
        }
        args.extend(oids);
        self.runner.write(args).map(drop)
    }

    /// Cherry-pick commits in order.
    ///
    /// # Errors
    /// * [`GitError`] when the pick conflicts or git fails.
    pub fn cherry_pick(&self, oids: &[&str]) -> Result<(), GitError> {
        let mut args = vec!["cherry-pick", "--allow-empty"];
        args.extend(oids);
        self.runner.write(args).map(drop)
    }

    /// Reset `HEAD`.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn reset(&self, target: &str, mode: ResetMode) -> Result<(), GitError> {
        self.runner.write(["reset", mode.flag(), target]).map(drop)
    }

    /// Create a `fixup!` commit for `oid` from the staged changes.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn create_fixup(&self, oid: &str) -> Result<(), GitError> {
        let fixup = format!("--fixup={oid}");
        self.runner.write(["commit", &fixup]).map(drop)
    }

    // ----- rebase -----

    /// Run an interactive rebase onto `base` with a prepared todo list (oldest first). A `None`
    /// base rewrites from the root commit.
    ///
    /// # Errors
    /// * [`GitError`] when the rebase stops on a conflict or git fails.
    pub fn rebase_interactive(
        &self,
        base: Option<&str>,
        todo: &[RebaseTodoItem],
    ) -> Result<(), GitError> {
        let lines: Vec<String> = todo
            .iter()
            .map(|item| format!("{} {}", item.action.keyword(), item.oid))
            .collect();
        self.run_rebase_todo(base, &lines.join("\n"), &[])
    }

    /// Rebase with `--autosquash` onto `base`, applying pending fixups.
    ///
    /// # Errors
    /// * [`GitError`] when the rebase stops or git fails.
    pub fn autosquash(&self, base: &str) -> Result<(), GitError> {
        self.runner
            .run(
                &GitInvocation::write([
                    "rebase",
                    "--interactive",
                    "--autosquash",
                    "--autostash",
                    base,
                ])
                .env("GIT_SEQUENCE_EDITOR", "true")
                .env("GIT_EDITOR", "true"),
            )
            .map(drop)
    }

    /// Rebase the current branch onto `upstream`.
    ///
    /// # Errors
    /// * [`GitError`] when the rebase stops or git fails.
    pub fn rebase(&self, upstream: &str) -> Result<(), GitError> {
        self.runner
            .run(
                &GitInvocation::write(["rebase", "--autostash", upstream])
                    .env("GIT_EDITOR", "true"),
            )
            .map(drop)
    }

    /// Continue the in-progress rebase, merge, cherry-pick or revert.
    ///
    /// # Errors
    /// * [`GitError`] when the operation stops again or git fails.
    pub fn continue_operation(&self) -> Result<(), GitError> {
        let command = match self.operation() {
            RepoOperation::Rebasing => vec!["rebase", "--continue"],
            RepoOperation::Merging => vec!["commit", "--no-edit"],
            RepoOperation::CherryPicking => vec!["cherry-pick", "--continue"],
            RepoOperation::Reverting => vec!["revert", "--continue"],
            RepoOperation::ApplyingMailbox => vec!["am", "--continue"],
            RepoOperation::Bisecting | RepoOperation::None => {
                return Err(GitError::Unsupported("nothing to continue".into()));
            }
        };
        self.runner
            .run(&GitInvocation::write(command).env("GIT_EDITOR", "true"))
            .map(drop)
    }

    /// Abort the in-progress operation.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn abort_operation(&self) -> Result<(), GitError> {
        let command = match self.operation() {
            RepoOperation::Rebasing => vec!["rebase", "--abort"],
            RepoOperation::Merging => vec!["merge", "--abort"],
            RepoOperation::CherryPicking => vec!["cherry-pick", "--abort"],
            RepoOperation::Reverting => vec!["revert", "--abort"],
            RepoOperation::ApplyingMailbox => vec!["am", "--abort"],
            RepoOperation::Bisecting => vec!["bisect", "reset"],
            RepoOperation::None => {
                return Err(GitError::Unsupported("nothing to abort".into()));
            }
        };
        self.runner.write(command).map(drop)
    }

    /// Skip the current commit of the in-progress rebase or cherry-pick.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn skip_operation(&self) -> Result<(), GitError> {
        let command = match self.operation() {
            RepoOperation::Rebasing => vec!["rebase", "--skip"],
            RepoOperation::CherryPicking => vec!["cherry-pick", "--skip"],
            RepoOperation::Reverting => vec!["revert", "--skip"],
            _ => return Err(GitError::Unsupported("nothing to skip".into())),
        };
        self.runner.write(command).map(drop)
    }

    /// Rewrite the todo list of an interactive rebase from `base`, replacing the `pick` line of
    /// each listed commit with the given lines and keeping others.
    fn rewrite_todo(
        &self,
        base: Option<&str>,
        replacements: &[(String, Vec<String>)],
    ) -> Result<(), GitError> {
        let commits = match base {
            Some(base) => self.commits_between(base, "HEAD")?,
            None => self.log(&LogQuery {
                limit: 100_000,
                ..LogQuery::default()
            })?,
        };
        let mut lines = Vec::new();
        for commit in commits.iter().rev() {
            if let Some((_, replacement)) = replacements.iter().find(|(oid, _)| *oid == commit.oid)
            {
                lines.extend(replacement.iter().cloned());
            } else {
                lines.push(format!("pick {}", commit.oid));
            }
        }
        self.run_rebase_todo(base, &lines.join("\n"), &[])
    }

    fn run_rebase_todo(
        &self,
        base: Option<&str>,
        todo: &str,
        extra: &[&str],
    ) -> Result<(), GitError> {
        let todo_file = self.paths.git_dir.join("BCODE_REBASE_TODO");
        std::fs::write(&todo_file, format!("{todo}\n"))?;
        let editor = format!("cp {}", shell_quote(&todo_file.to_string_lossy()));
        let mut args = vec!["rebase", "--interactive", "--autostash", "--keep-empty"];
        args.extend(extra);
        args.push(base.unwrap_or("--root"));
        let result = self.runner.run(
            &GitInvocation::write(args)
                .env("GIT_SEQUENCE_EDITOR", editor)
                .env("GIT_EDITOR", "true"),
        );
        let _ = std::fs::remove_file(&todo_file);
        result.map(drop)
    }

    // ----- branches -----

    /// Check out a branch, tag or commit.
    ///
    /// # Errors
    /// * [`GitError`] when local changes would be overwritten or git fails.
    pub fn checkout(&self, target: &str) -> Result<(), GitError> {
        self.runner.write(["checkout", target]).map(drop)
    }

    /// Check out a remote branch as a new tracking local branch.
    ///
    /// # Errors
    /// * [`GitError`] when the local branch exists or git fails.
    pub fn checkout_remote_branch(&self, remote_branch: &RemoteBranch) -> Result<(), GitError> {
        let full = remote_branch.full_name();
        self.runner
            .write(["checkout", "--track", "-b", &remote_branch.name, &full])
            .map(drop)
    }

    /// Create a branch at `start` (default `HEAD`), optionally checking it out.
    ///
    /// # Errors
    /// * [`GitError`] when the name is invalid or exists.
    pub fn create_branch(
        &self,
        name: &str,
        start: Option<&str>,
        checkout: bool,
    ) -> Result<(), GitError> {
        let mut args = if checkout {
            vec!["checkout", "-b", name]
        } else {
            vec!["branch", name]
        };
        args.extend(start);
        self.runner.write(args).map(drop)
    }

    /// Rename a branch.
    ///
    /// # Errors
    /// * [`GitError`] when the new name is invalid or exists.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<(), GitError> {
        self.runner.write(["branch", "-m", old, new]).map(drop)
    }

    /// Delete a local branch.
    ///
    /// # Errors
    /// * [`GitError`] when the branch is unmerged and `force` is false.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<(), GitError> {
        let flag = if force { "-D" } else { "-d" };
        self.runner.write(["branch", flag, name]).map(drop)
    }

    /// Delete a branch on a remote.
    ///
    /// # Errors
    /// * [`GitError`] when the push fails.
    pub fn delete_remote_branch(&self, remote: &str, name: &str) -> Result<(), GitError> {
        self.runner
            .write(["push", remote, "--delete", name])
            .map(drop)
    }

    /// Merge `target` into the current branch.
    ///
    /// # Errors
    /// * [`GitError`] when the merge conflicts or git fails.
    pub fn merge(&self, target: &str, squash: bool, no_ff: bool) -> Result<(), GitError> {
        let mut args = vec!["merge", "--no-edit"];
        if squash {
            args.push("--squash");
        }
        if no_ff {
            args.push("--no-ff");
        }
        args.push(target);
        self.runner.write(args).map(drop)
    }

    /// Set or unset the upstream of a branch.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn set_upstream(&self, branch: &str, upstream: Option<&str>) -> Result<(), GitError> {
        upstream
            .map_or_else(
                || self.runner.write(["branch", "--unset-upstream", branch]),
                |upstream| {
                    let flag = format!("--set-upstream-to={upstream}");
                    self.runner.write(["branch", &flag, branch])
                },
            )
            .map(drop)
    }

    /// Fast-forward a local branch to its upstream, fetching the upstream first.
    ///
    /// A branch checked out here or in another worktree is merged (fast-forward only) in
    /// that worktree, so its files move with it; any other branch only has its ref moved.
    ///
    /// # Errors
    /// * [`GitError`] when the branch has no upstream, the fetch fails, or the branch has
    ///   diverged from its upstream or its worktree's changes are in the way.
    pub fn fast_forward(&self, branch: &Branch) -> Result<(), GitError> {
        let upstream = branch
            .upstream
            .as_deref()
            .ok_or_else(|| GitError::Unsupported(format!("{} has no upstream", branch.name)))?;
        let (remote, remote_branch) = upstream
            .split_once('/')
            .ok_or_else(|| GitError::Unsupported(format!("unexpected upstream {upstream}")))?;
        self.runner.write(["fetch", remote, remote_branch])?;
        if branch.is_head {
            return self
                .runner
                .write(["merge", "--ff-only", upstream])
                .map(drop);
        }
        let checked_out = self
            .worktrees()?
            .into_iter()
            .find(|worktree| worktree.branch.as_deref() == Some(branch.name.as_str()));
        if let Some(worktree) = checked_out {
            return self
                .runner
                .write([
                    OsStr::new("-C"),
                    worktree.path.as_os_str(),
                    OsStr::new("merge"),
                    OsStr::new("--ff-only"),
                    OsStr::new(upstream),
                ])
                .map(drop);
        }
        // Not checked out anywhere: a local fetch moves the ref, refusing anything but a
        // fast-forward.
        let refspec = format!("refs/remotes/{upstream}:refs/heads/{}", branch.name);
        self.runner
            .write(["fetch", "--quiet", ".", &refspec])
            .map(drop)
    }

    // ----- remotes -----

    /// Fetch from one remote or all remotes, pruning deleted branches.
    ///
    /// # Errors
    /// * [`GitError`] when the fetch fails.
    pub fn fetch(&self, remote: Option<&str>) -> Result<(), GitError> {
        let mut args = vec!["fetch", "--prune"];
        match remote {
            Some(remote) => args.push(remote),
            None => args.push("--all"),
        }
        self.runner.write(args).map(drop)
    }

    /// Pull the current branch from its upstream.
    ///
    /// # Errors
    /// * [`GitError`] when the pull fails or conflicts.
    pub fn pull(&self, rebase: Option<bool>) -> Result<(), GitError> {
        let mut args = vec!["pull", "--no-edit"];
        match rebase {
            Some(true) => args.push("--rebase"),
            Some(false) => args.push("--no-rebase"),
            None => {}
        }
        self.runner
            .run(&GitInvocation::write(args).env("GIT_EDITOR", "true"))
            .map(drop)
    }

    /// Push.
    ///
    /// # Errors
    /// * [`GitError`] when the push is rejected or fails.
    pub fn push(&self, options: &PushOptions) -> Result<(), GitError> {
        let mut args = vec!["push".to_owned()];
        if options.force_with_lease {
            args.push("--force-with-lease".into());
        }
        if options.force {
            args.push("--force".into());
        }
        if options.set_upstream {
            args.push("--set-upstream".into());
        }
        if options.tags {
            args.push("--tags".into());
        }
        args.extend(options.remote.clone());
        args.extend(options.refspec.clone());
        self.runner.write(args).map(drop)
    }

    /// Add a remote.
    ///
    /// # Errors
    /// * [`GitError`] when the name exists or is invalid.
    pub fn add_remote(&self, name: &str, url: &str) -> Result<(), GitError> {
        self.runner.write(["remote", "add", name, url]).map(drop)
    }

    /// Remove a remote.
    ///
    /// # Errors
    /// * [`GitError`] when the remote does not exist.
    pub fn remove_remote(&self, name: &str) -> Result<(), GitError> {
        self.runner.write(["remote", "remove", name]).map(drop)
    }

    /// Rename a remote.
    ///
    /// # Errors
    /// * [`GitError`] when the remote does not exist or the name is taken.
    pub fn rename_remote(&self, old: &str, new: &str) -> Result<(), GitError> {
        self.runner.write(["remote", "rename", old, new]).map(drop)
    }

    /// Change a remote URL.
    ///
    /// # Errors
    /// * [`GitError`] when the remote does not exist.
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<(), GitError> {
        self.runner
            .write(["remote", "set-url", name, url])
            .map(drop)
    }

    // ----- tags -----

    /// Create a lightweight or annotated tag.
    ///
    /// # Errors
    /// * [`GitError`] when the tag exists or the name is invalid.
    pub fn create_tag(
        &self,
        name: &str,
        target: &str,
        message: Option<&str>,
    ) -> Result<(), GitError> {
        let invocation = message.map_or_else(
            || GitInvocation::write(["tag", name, target]),
            |message| {
                GitInvocation::write(["tag", "--annotate", "--file=-", name, target])
                    .stdin(message.as_bytes())
            },
        );
        self.runner.run(&invocation).map(drop)
    }

    /// Delete a local tag.
    ///
    /// # Errors
    /// * [`GitError`] when the tag does not exist.
    pub fn delete_tag(&self, name: &str) -> Result<(), GitError> {
        self.runner.write(["tag", "--delete", name]).map(drop)
    }

    /// Push a tag to a remote.
    ///
    /// # Errors
    /// * [`GitError`] when the push fails.
    pub fn push_tag(&self, remote: &str, name: &str) -> Result<(), GitError> {
        let refspec = format!("refs/tags/{name}");
        self.runner.write(["push", remote, &refspec]).map(drop)
    }

    // ----- stash -----

    /// Stash changes.
    ///
    /// # Errors
    /// * [`GitError`] when there is nothing to stash or git fails.
    pub fn stash_push(&self, options: &StashOptions) -> Result<(), GitError> {
        let mut args = vec!["stash".to_owned(), "push".to_owned()];
        if options.include_untracked {
            args.push("--include-untracked".into());
        }
        if options.staged {
            args.push("--staged".into());
        }
        if options.keep_index {
            args.push("--keep-index".into());
        }
        if let Some(message) = &options.message {
            args.push(format!("--message={message}"));
        }
        if !options.paths.is_empty() {
            args.push("--".into());
            args.extend(options.paths.iter().cloned());
        }
        self.runner.write(args).map(drop)
    }

    /// Apply a stash, optionally dropping it.
    ///
    /// # Errors
    /// * [`GitError`] when the stash conflicts or git fails.
    pub fn stash_apply(&self, selector: &str, pop: bool) -> Result<(), GitError> {
        let command = if pop { "pop" } else { "apply" };
        self.runner
            .write(["stash", command, "--index", selector])
            .or_else(|_| {
                // Retry without restoring the index when it cannot be reinstated.
                self.runner.write(["stash", command, selector])
            })
            .map(drop)
    }

    /// Drop a stash.
    ///
    /// # Errors
    /// * [`GitError`] when the entry does not exist.
    pub fn stash_drop(&self, selector: &str) -> Result<(), GitError> {
        self.runner.write(["stash", "drop", selector]).map(drop)
    }

    /// Rename a stash entry.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn stash_rename(&self, entry: &StashEntry, message: &str) -> Result<(), GitError> {
        self.stash_drop(&entry.selector())?;
        let message = format!("--message={message}");
        self.runner
            .write(["stash", "store", &message, &entry.oid])
            .map(drop)
    }

    /// Create a branch from a stash entry.
    ///
    /// # Errors
    /// * [`GitError`] when the branch exists or the stash conflicts.
    pub fn stash_branch(&self, name: &str, selector: &str) -> Result<(), GitError> {
        self.runner
            .write(["stash", "branch", name, selector])
            .map(drop)
    }

    // ----- bisect -----

    /// Start bisecting between a bad and a good revision.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn bisect_start(&self, bad: &str, good: &str) -> Result<(), GitError> {
        self.runner.write(["bisect", "start", bad, good]).map(drop)
    }

    /// Mark a revision during bisect (`good`, `bad` or `skip`).
    ///
    /// # Errors
    /// * [`GitError`] when not bisecting or git fails.
    pub fn bisect_mark(&self, term: &str, revision: &str) -> Result<String, GitError> {
        self.runner.write(["bisect", term, revision])
    }

    /// End bisecting.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn bisect_reset(&self) -> Result<(), GitError> {
        self.runner.write(["bisect", "reset"]).map(drop)
    }

    // ----- worktrees and submodules -----

    /// Add a worktree for a new or existing branch.
    ///
    /// # Errors
    /// * [`GitError`] when the path exists or git fails.
    pub fn add_worktree(
        &self,
        path: &Path,
        branch: &str,
        create: bool,
        start: Option<&str>,
    ) -> Result<(), GitError> {
        let path = path.to_string_lossy();
        let mut args = vec!["worktree", "add"];
        if create {
            args.extend(["-b", branch, &path]);
            args.extend(start);
        } else {
            args.extend([&path, branch]);
        }
        self.runner.write(args).map(drop)
    }

    /// Remove a worktree.
    ///
    /// # Errors
    /// * [`GitError`] when the worktree is dirty and `force` is false.
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), GitError> {
        let path = path.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path);
        self.runner.write(args).map(drop)
    }

    /// Initialize and update submodules recursively.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    pub fn update_submodules(&self) -> Result<(), GitError> {
        self.runner
            .write(["submodule", "update", "--init", "--recursive"])
            .map(drop)
    }

    // ----- helpers -----

    fn has_head(&self) -> bool {
        self.runner
            .run(&GitInvocation::read([
                "rev-parse",
                "--verify",
                "--quiet",
                "HEAD",
            ]))
            .is_ok()
    }

    fn is_merge(&self, oid: &str) -> bool {
        let parent = format!("{oid}^2");
        self.runner
            .run(&GitInvocation::read([
                "rev-parse",
                "--verify",
                "--quiet",
                &parent,
            ]))
            .is_ok()
    }

    fn with_paths(&self, command: &[&str], paths: &[&str]) -> Result<(), GitError> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = command.to_vec();
        args.push("--");
        args.extend(paths);
        self.runner.write(args).map(drop)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// bcode's draft commit message, in the (per-worktree) git directory.
pub const BCODE_COMMIT_DRAFT: &str = "BCODE_GIT_COMMIT_MSG";
/// lazygit's unsent commit message, in the git directory.
pub const LAZYGIT_COMMIT_DRAFT: &str = "LAZYGIT_PENDING_COMMIT";
/// Larger files are not messages; they are ignored rather than loaded.
const MAX_MESSAGE_BYTES: u64 = 1024 * 1024;

fn read_message_file(path: &Path) -> Result<Option<String>, GitError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_MESSAGE_BYTES => {
            let bytes = std::fs::read(path)?;
            Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_if_present(path: &Path) -> Result<(), GitError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
