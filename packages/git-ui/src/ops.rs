//! Typed repository operations run off the input thread.

use std::collections::BTreeSet;
use std::path::PathBuf;

use bcode_git::models::{
    Branch, DiffFile, DiffLineId, HeadState, RebaseTodoItem, RemoteBranch, ResetMode, StashEntry,
};
use bcode_git::{
    ApplyTarget, CommitOptions, ConflictSide, GitError, PushOptions, Repository, StashOptions,
};

/// A repository mutation.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub enum Op {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    StageAll,
    UnstageAll,
    Discard {
        tracked: Vec<String>,
        untracked: Vec<String>,
    },
    DiscardUnstaged(Vec<String>),
    StageLines {
        file: DiffFile,
        ids: BTreeSet<DiffLineId>,
        intent_to_add: Option<String>,
    },
    UnstageLines {
        file: DiffFile,
        ids: BTreeSet<DiffLineId>,
    },
    DiscardLines {
        file: DiffFile,
        ids: BTreeSet<DiffLineId>,
    },
    ApplyPatch {
        patch: String,
        target: ApplyTarget,
        reverse: bool,
    },
    Commit(CommitOptions),
    Reword {
        oid: String,
        message: String,
    },
    Checkout(String),
    CheckoutRemote(RemoteBranch),
    CheckoutPath {
        revision: String,
        path: String,
    },
    CreateBranch {
        name: String,
        start: Option<String>,
        checkout: bool,
    },
    RenameBranch {
        old: String,
        new: String,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    DeleteRemoteBranch {
        remote: String,
        name: String,
    },
    Merge {
        target: String,
        squash: bool,
        no_ff: bool,
    },
    Rebase(String),
    SetUpstream {
        branch: String,
        upstream: Option<String>,
    },
    FastForward(Branch),
    Fetch(Option<String>),
    Pull {
        rebase: Option<bool>,
    },
    Push(PushOptions),
    AddRemote {
        name: String,
        url: String,
    },
    RemoveRemote(String),
    SetRemoteUrl {
        name: String,
        url: String,
    },
    CreateTag {
        name: String,
        target: String,
        message: Option<String>,
    },
    DeleteTag(String),
    PushTag {
        remote: String,
        name: String,
    },
    StashPush(StashOptions),
    StashApply {
        selector: String,
        pop: bool,
    },
    StashDrop(String),
    StashRename {
        entry: StashEntry,
        message: String,
    },
    StashBranch {
        name: String,
        selector: String,
    },
    Reset {
        target: String,
        mode: ResetMode,
    },
    Revert(Vec<String>),
    CherryPick(Vec<String>),
    CreateFixup(String),
    AmendInto(String),
    Autosquash(String),
    RebaseTodo {
        base: Option<String>,
        todo: Vec<RebaseTodoItem>,
    },
    Continue,
    Abort,
    Skip,
    BisectStart {
        bad: String,
        good: String,
    },
    BisectMark {
        term: String,
        revision: String,
    },
    BisectReset,
    AddWorktree {
        path: PathBuf,
        branch: String,
        create: bool,
    },
    RemoveWorktree {
        path: PathBuf,
        force: bool,
    },
    UpdateSubmodules,
    Ignore(String),
    Exclude(String),
    ResolveConflict {
        path: String,
        side: ConflictSide,
    },
    Shell(String),
    Copy(String),
    BisectBegin,
    Restore {
        position: HeadPosition,
        journal: JournalMove,
    },
}

/// Direction an undo-journal restore moves entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalMove {
    /// Undo the latest entry.
    Undo,
    /// Redo the latest undone entry.
    Redo,
}

impl Op {
    /// Short label for toasts.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Stage(paths) => format!("Staged {}", plural(paths.len(), "file")),
            Self::Unstage(paths) => format!("Unstaged {}", plural(paths.len(), "file")),
            Self::StageAll => "Staged all changes".into(),
            Self::UnstageAll => "Unstaged all changes".into(),
            Self::Discard { tracked, untracked } => {
                format!(
                    "Discarded {}",
                    plural(tracked.len() + untracked.len(), "file")
                )
            }
            Self::DiscardUnstaged(paths) => {
                format!(
                    "Discarded unstaged changes in {}",
                    plural(paths.len(), "file")
                )
            }
            Self::StageLines { ids, .. } => format!("Staged {}", plural(ids.len(), "line")),
            Self::UnstageLines { ids, .. } => format!("Unstaged {}", plural(ids.len(), "line")),
            Self::DiscardLines { ids, .. } => format!("Discarded {}", plural(ids.len(), "line")),
            Self::ApplyPatch { .. } => "Applied patch".into(),
            Self::Commit(options) if options.amend => "Amended commit".into(),
            Self::Commit(_) => "Committed".into(),
            Self::Reword { .. } => "Reworded commit".into(),
            Self::Checkout(target) => format!("Checked out {target}"),
            Self::CheckoutRemote(branch) => format!("Checked out {}", branch.name),
            Self::CheckoutPath { path, .. } => format!("Checked out {path}"),
            Self::CreateBranch { name, .. } => format!("Created {name}"),
            Self::RenameBranch { new, .. } => format!("Renamed to {new}"),
            Self::DeleteBranch { name, .. } => format!("Deleted {name}"),
            Self::DeleteRemoteBranch { remote, name } => format!("Deleted {remote}/{name}"),
            Self::Merge { target, .. } => format!("Merged {target}"),
            Self::Rebase(target) => format!("Rebased onto {target}"),
            Self::SetUpstream {
                upstream: Some(upstream),
                ..
            } => format!("Upstream set to {upstream}"),
            Self::SetUpstream { upstream: None, .. } => "Upstream removed".into(),
            Self::FastForward(branch) => format!("Fast-forwarded {}", branch.name),
            Self::Fetch(Some(remote)) => format!("Fetched {remote}"),
            Self::Fetch(None) => "Fetched".into(),
            Self::Pull { .. } => "Pulled".into(),
            Self::Push(options) if options.force_with_lease || options.force => {
                "Force pushed".into()
            }
            Self::Push(_) => "Pushed".into(),
            Self::AddRemote { name, .. } => format!("Added remote {name}"),
            Self::RemoveRemote(name) => format!("Removed remote {name}"),
            Self::SetRemoteUrl { name, .. } => format!("Updated remote {name}"),
            Self::CreateTag { name, .. } => format!("Tagged {name}"),
            Self::DeleteTag(name) => format!("Deleted tag {name}"),
            Self::PushTag { name, .. } => format!("Pushed tag {name}"),
            Self::StashPush(_) => "Stashed changes".into(),
            Self::StashApply { pop: true, .. } => "Popped stash".into(),
            Self::StashApply { .. } => "Applied stash".into(),
            Self::StashDrop(_) => "Dropped stash".into(),
            Self::StashRename { .. } => "Renamed stash".into(),
            Self::StashBranch { name, .. } => format!("Created {name} from stash"),
            Self::Reset { mode, .. } => format!("Reset ({})", mode.flag().trim_start_matches("--")),
            Self::Revert(oids) => format!("Reverted {}", plural(oids.len(), "commit")),
            Self::CherryPick(oids) => format!("Cherry-picked {}", plural(oids.len(), "commit")),
            Self::CreateFixup(_) => "Created fixup commit".into(),
            Self::AmendInto(_) => "Amended staged changes into commit".into(),
            Self::Autosquash(_) => "Applied fixup commits".into(),
            Self::RebaseTodo { .. } => "Rewrote history".into(),
            Self::Continue => "Continued".into(),
            Self::Abort => "Aborted".into(),
            Self::Skip => "Skipped".into(),
            Self::BisectStart { .. } | Self::BisectBegin => "Bisect started".into(),
            Self::BisectMark { term, .. } => format!("Marked {term}"),
            Self::BisectReset => "Bisect finished".into(),
            Self::AddWorktree { path, .. } => format!("Created worktree {}", path.display()),
            Self::RemoveWorktree { path, .. } => format!("Removed worktree {}", path.display()),
            Self::UpdateSubmodules => "Updated submodules".into(),
            Self::Ignore(pattern) => format!("Ignored {pattern}"),
            Self::Exclude(pattern) => format!("Excluded {pattern}"),
            Self::ResolveConflict { path, .. } => format!("Resolved {path}"),
            Self::Shell(command) => format!("Ran {command}"),
            Self::Copy(_) => "Copied to clipboard".into(),
            Self::Restore {
                journal: JournalMove::Undo,
                ..
            } => "Undone".into(),
            Self::Restore {
                journal: JournalMove::Redo,
                ..
            } => "Redone".into(),
        }
    }

    /// Whether the operation contacts a remote (shown with a progress indicator).
    #[must_use]
    pub const fn is_network(&self) -> bool {
        matches!(
            self,
            Self::Fetch(_)
                | Self::Pull { .. }
                | Self::Push(_)
                | Self::PushTag { .. }
                | Self::DeleteRemoteBranch { .. }
                | Self::FastForward(_)
        )
    }

    /// Undo-journal movement performed by this operation.
    #[must_use]
    pub const fn journal(&self) -> Option<JournalMove> {
        match self {
            Self::Restore { journal, .. } => Some(*journal),
            _ => None,
        }
    }

    /// Whether the operation can move `HEAD` and should be recorded for undo.
    #[must_use]
    pub const fn moves_head(&self) -> bool {
        matches!(
            self,
            Self::Commit(_)
                | Self::Reword { .. }
                | Self::Checkout(_)
                | Self::CheckoutRemote(_)
                | Self::Merge { .. }
                | Self::Rebase(_)
                | Self::Pull { .. }
                | Self::Reset { .. }
                | Self::Revert(_)
                | Self::CherryPick(_)
                | Self::CreateFixup(_)
                | Self::AmendInto(_)
                | Self::Autosquash(_)
                | Self::RebaseTodo { .. }
        )
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Where `HEAD` was, for undo and redo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadPosition {
    /// Attached branch.
    pub branch: Option<String>,
    /// Commit.
    pub oid: String,
}

impl HeadPosition {
    /// Capture the current position.
    #[must_use]
    pub fn capture(repo: &Repository) -> Option<Self> {
        let oid = repo.rev_parse("HEAD").ok()?;
        let branch = match repo.head().ok()? {
            HeadState::Branch { name } => Some(name),
            _ => None,
        };
        Some(Self { branch, oid })
    }

    /// Return the repository to this position. Uses `reset --keep` on the same branch so local
    /// changes that would be overwritten abort the undo instead of being lost.
    ///
    /// # Errors
    /// * [`GitError`] when git refuses the move.
    pub fn restore(&self, repo: &Repository) -> Result<(), GitError> {
        let current = Self::capture(repo);
        let same_branch = current
            .as_ref()
            .is_some_and(|current| current.branch == self.branch);
        match (&self.branch, same_branch) {
            (_, true) => repo.reset(&self.oid, ResetMode::Keep),
            (Some(branch), false) => {
                repo.checkout(branch)?;
                if repo.rev_parse("HEAD")? == self.oid {
                    Ok(())
                } else {
                    repo.reset(&self.oid, ResetMode::Keep)
                }
            }
            (None, false) => repo.checkout(&self.oid),
        }
    }
}

/// Index and worktree operations; `None` for other operations.
fn run_worktree_op(repo: &Repository, op: &Op) -> Option<Result<(), GitError>> {
    Some(match op {
        Op::Stage(paths) => repo.stage(&refs(paths)),
        Op::Unstage(paths) => repo.unstage(&refs(paths)),
        Op::StageAll => repo.stage_all(),
        Op::UnstageAll => repo.unstage_all(),
        Op::Discard { tracked, untracked } => repo.discard(&refs(tracked), &refs(untracked)),
        Op::DiscardUnstaged(paths) => repo.discard_unstaged(&refs(paths)),
        Op::StageLines {
            file,
            ids,
            intent_to_add,
        } => {
            if let Some(path) = intent_to_add {
                return Some(repo.intent_to_add(&[path]).and_then(|()| {
                    let file = repo
                        .diff(&bcode_git::DiffTarget::Unstaged, &[path], 3)?
                        .into_iter()
                        .next()
                        .ok_or_else(|| GitError::Unsupported("file has no changes".into()))?;
                    repo.stage_lines(&file, ids)
                }));
            }
            repo.stage_lines(file, ids)
        }
        Op::UnstageLines { file, ids } => repo.unstage_lines(file, ids),
        Op::DiscardLines { file, ids } => repo.discard_lines(file, ids),
        Op::ApplyPatch {
            patch,
            target,
            reverse,
        } => repo.apply_patch(patch, *target, *reverse),
        Op::Commit(options) => repo.commit(options),
        _ => return None,
    })
}

/// Run an operation, returning optional output to show.
///
/// # Errors
/// Returns git's user-facing message.
pub fn run(repo: &Repository, op: &Op) -> Result<Option<String>, String> {
    let message = |error: GitError| error.message();
    if let Some(result) = run_worktree_op(repo, op) {
        return result.map(|()| None).map_err(message);
    }
    match op {
        Op::Reword { oid, message } => repo.reword(oid, message),
        Op::Checkout(target) => repo.checkout(target),
        Op::CheckoutRemote(branch) => repo.checkout_remote_branch(branch),
        Op::CheckoutPath { revision, path } => repo.checkout_path(revision, path),
        Op::CreateBranch {
            name,
            start,
            checkout,
        } => repo.create_branch(name, start.as_deref(), *checkout),
        Op::RenameBranch { old, new } => repo.rename_branch(old, new),
        Op::DeleteBranch { name, force } => repo.delete_branch(name, *force),
        Op::DeleteRemoteBranch { remote, name } => repo.delete_remote_branch(remote, name),
        Op::Merge {
            target,
            squash,
            no_ff,
        } => repo.merge(target, *squash, *no_ff),
        Op::Rebase(target) => repo.rebase(target),
        Op::SetUpstream { branch, upstream } => repo.set_upstream(branch, upstream.as_deref()),
        Op::FastForward(branch) => repo.fast_forward(branch),
        Op::Fetch(remote) => repo.fetch(remote.as_deref()),
        Op::Pull { rebase } => repo.pull(*rebase),
        Op::Push(options) => repo.push(options),
        Op::AddRemote { name, url } => repo.add_remote(name, url),
        Op::RemoveRemote(name) => repo.remove_remote(name),
        Op::SetRemoteUrl { name, url } => repo.set_remote_url(name, url),
        Op::CreateTag {
            name,
            target,
            message,
        } => repo.create_tag(name, target, message.as_deref()),
        Op::DeleteTag(name) => repo.delete_tag(name),
        Op::PushTag { remote, name } => repo.push_tag(remote, name),
        Op::StashPush(options) => repo.stash_push(options),
        Op::StashApply { selector, pop } => repo.stash_apply(selector, *pop),
        Op::StashDrop(selector) => repo.stash_drop(selector),
        Op::StashRename { entry, message } => repo.stash_rename(entry, message),
        Op::StashBranch { name, selector } => repo.stash_branch(name, selector),
        Op::Reset { target, mode } => repo.reset(target, *mode),
        Op::Revert(oids) => repo.revert(&refs(oids)),
        Op::CherryPick(oids) => repo.cherry_pick(&refs(oids)),
        Op::CreateFixup(oid) => repo.create_fixup(oid),
        Op::AmendInto(oid) => repo.amend_into(oid),
        Op::Autosquash(base) => repo.autosquash(base),
        Op::RebaseTodo { base, todo } => repo.rebase_interactive(base.as_deref(), todo),
        Op::Continue => repo.continue_operation(),
        Op::Abort => repo.abort_operation(),
        Op::Skip => repo.skip_operation(),
        Op::BisectStart { bad, good } => repo.bisect_start(bad, good),
        Op::BisectMark { term, revision } => {
            return repo
                .bisect_mark(term, revision)
                .map(|output| first_line_with(&output, "is the first bad commit"))
                .map_err(message);
        }
        Op::BisectReset => repo.bisect_reset(),
        Op::AddWorktree {
            path,
            branch,
            create,
        } => repo.add_worktree(path, branch, *create, None),
        Op::RemoveWorktree { path, force } => repo.remove_worktree(path, *force),
        Op::UpdateSubmodules => repo.update_submodules(),
        Op::Ignore(pattern) => repo.ignore(pattern),
        Op::Exclude(pattern) => repo.exclude(pattern),
        Op::ResolveConflict { path, side } => repo.resolve_conflict(path, *side),
        Op::Shell(command) => return run_shell(repo, command).map(Some),
        Op::Stage(_)
        | Op::Unstage(_)
        | Op::StageAll
        | Op::UnstageAll
        | Op::Discard { .. }
        | Op::DiscardUnstaged(_)
        | Op::StageLines { .. }
        | Op::UnstageLines { .. }
        | Op::DiscardLines { .. }
        | Op::ApplyPatch { .. }
        | Op::Commit(_) => unreachable!("handled by run_worktree_op"),
        Op::Copy(text) => return copy_to_clipboard(text).map(|()| None),
        Op::BisectBegin => repo.runner().write(["bisect", "start"]).map(drop),
        Op::Restore { position, .. } => position.restore(repo),
    }
    .map(|()| None)
    .map_err(message)
}

fn refs(paths: &[String]) -> Vec<&str> {
    paths.iter().map(String::as_str).collect()
}

fn first_line_with(output: &str, needle: &str) -> Option<String> {
    output.contains(needle).then(|| output.trim().to_owned())
}

/// Run a shell command in the repository root, capturing combined output.
///
/// # Errors
/// Returns the command's output when it fails.
pub fn run_shell(repo: &Repository, command: &str) -> Result<String, String> {
    let output = shell_command(command)
        .current_dir(repo.root())
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(text.trim_end().to_owned())
    } else {
        Err(if text.trim().is_empty() {
            format!("{command} failed with {}", output.status)
        } else {
            text.trim_end().to_owned()
        })
    }
}

/// A platform shell invocation of `command`.
#[must_use]
pub fn shell_command(command: &str) -> std::process::Command {
    if cfg!(windows) {
        let mut process = std::process::Command::new("cmd");
        process.args(["/C", command]);
        process
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
        let mut process = std::process::Command::new(shell);
        process.args(["-c", command]);
        process
    }
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write as _;
    let candidates: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else if cfg!(windows) {
        &[&["clip"]]
    } else {
        &[
            &["wl-copy"],
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };
    for candidate in candidates {
        let Ok(mut child) = std::process::Command::new(candidate[0])
            .args(&candidate[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        return child
            .wait()
            .map_err(|error| error.to_string())
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err(format!("{} failed", candidate[0]))
                }
            });
    }
    Err("no clipboard command found (pbcopy, wl-copy, xclip or xsel)".into())
}
