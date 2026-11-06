#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable git repository models.
//!
//! These types describe repository state as reported by the `git` command line. They carry no
//! process, terminal, or UI behavior so that frontends, plugins and tools can share them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where `HEAD` points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HeadState {
    /// `HEAD` names a local branch with at least one commit.
    Branch {
        /// Short branch name.
        name: String,
    },
    /// `HEAD` points directly at a commit.
    Detached {
        /// Full object identifier.
        oid: String,
    },
    /// `HEAD` names a branch that has no commits yet.
    Unborn {
        /// Short branch name.
        name: String,
    },
}

impl HeadState {
    /// Branch name when `HEAD` is attached.
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        match self {
            Self::Branch { name } | Self::Unborn { name } => Some(name),
            Self::Detached { .. } => None,
        }
    }
}

/// Multi-step operation in progress in a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoOperation {
    /// No operation in progress.
    #[default]
    None,
    /// A merge stopped for conflict resolution or commit.
    Merging,
    /// A rebase is in progress.
    Rebasing,
    /// A cherry-pick is in progress.
    CherryPicking,
    /// A revert is in progress.
    Reverting,
    /// A bisect session is active.
    Bisecting,
    /// A mailbox apply (`git am`) is in progress.
    ApplyingMailbox,
}

impl RepoOperation {
    /// Human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Merging => "merging",
            Self::Rebasing => "rebasing",
            Self::CherryPicking => "cherry-picking",
            Self::Reverting => "reverting",
            Self::Bisecting => "bisecting",
            Self::ApplyingMailbox => "applying patches",
        }
    }
}

/// Paths identifying a discovered repository worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPaths {
    /// Worktree root (`--show-toplevel`).
    pub root: PathBuf,
    /// Per-worktree git directory (`--git-dir`).
    pub git_dir: PathBuf,
    /// Shared git directory (`--git-common-dir`).
    pub common_dir: PathBuf,
}

/// Branch header from `git status --porcelain=v2 --branch`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BranchStatus {
    /// Current commit, absent on an unborn branch.
    pub oid: Option<String>,
    /// Current branch, absent when detached.
    pub head: Option<String>,
    /// Upstream tracking ref, if any.
    pub upstream: Option<String>,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
}

/// One side (index or worktree) of a status entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// No change on this side.
    Unmodified,
    /// Content changed.
    Modified,
    /// Object type changed (for example file to symlink).
    TypeChanged,
    /// Newly added.
    Added,
    /// Deleted.
    Deleted,
    /// Renamed.
    Renamed,
    /// Copied.
    Copied,
    /// Unmerged (conflicted).
    Unmerged,
}

impl ChangeKind {
    /// Decode a porcelain status letter.
    #[must_use]
    pub const fn from_code(code: char) -> Self {
        match code {
            'M' => Self::Modified,
            'T' => Self::TypeChanged,
            'A' => Self::Added,
            'D' => Self::Deleted,
            'R' => Self::Renamed,
            'C' => Self::Copied,
            'U' => Self::Unmerged,
            _ => Self::Unmodified,
        }
    }

    /// Porcelain status letter.
    #[must_use]
    pub const fn code(self) -> char {
        match self {
            Self::Unmodified => ' ',
            Self::Modified => 'M',
            Self::TypeChanged => 'T',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::Unmerged => 'U',
        }
    }
}

/// Conflict shape for an unmerged entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both sides deleted.
    BothDeleted,
    /// Added by us only.
    AddedByUs,
    /// Deleted by them.
    DeletedByThem,
    /// Added by them only.
    AddedByThem,
    /// Deleted by us.
    DeletedByUs,
    /// Both sides added.
    BothAdded,
    /// Both sides modified.
    BothModified,
}

impl ConflictKind {
    /// Decode a porcelain `XY` unmerged code.
    #[must_use]
    pub fn from_codes(codes: &str) -> Option<Self> {
        Some(match codes {
            "DD" => Self::BothDeleted,
            "AU" => Self::AddedByUs,
            "UD" => Self::DeletedByThem,
            "UA" => Self::AddedByThem,
            "DU" => Self::DeletedByUs,
            "AA" => Self::BothAdded,
            "UU" => Self::BothModified,
            _ => return None,
        })
    }
}

/// Classification of a status entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusEntryKind {
    /// Tracked path with ordinary changes.
    Ordinary,
    /// Tracked path that was renamed or copied.
    RenamedOrCopied,
    /// Conflicted path.
    Unmerged(ConflictKind),
    /// Untracked path.
    Untracked,
    /// Ignored path (only when requested).
    Ignored,
}

/// One changed path from `git status --porcelain=v2`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    /// Repository-relative path (new path for renames).
    pub path: String,
    /// Original path for renames and copies.
    pub orig_path: Option<String>,
    /// Entry classification.
    pub kind: StatusEntryKind,
    /// Change between `HEAD` and the index.
    pub index: ChangeKind,
    /// Change between the index and the worktree.
    pub worktree: ChangeKind,
    /// Whether the path is a submodule.
    pub submodule: bool,
}

impl StatusEntry {
    /// Whether the index differs from `HEAD` for this path.
    #[must_use]
    pub fn has_staged(&self) -> bool {
        matches!(
            self.kind,
            StatusEntryKind::Ordinary | StatusEntryKind::RenamedOrCopied
        ) && self.index != ChangeKind::Unmodified
    }

    /// Whether the worktree differs from the index, including untracked files.
    #[must_use]
    pub fn has_unstaged(&self) -> bool {
        match self.kind {
            StatusEntryKind::Untracked | StatusEntryKind::Unmerged(_) => true,
            StatusEntryKind::Ignored => false,
            StatusEntryKind::Ordinary | StatusEntryKind::RenamedOrCopied => {
                self.worktree != ChangeKind::Unmodified
            }
        }
    }

    /// Whether the path is conflicted.
    #[must_use]
    pub const fn is_conflicted(&self) -> bool {
        matches!(self.kind, StatusEntryKind::Unmerged(_))
    }

    /// Whether the path is untracked.
    #[must_use]
    pub const fn is_untracked(&self) -> bool {
        matches!(self.kind, StatusEntryKind::Untracked)
    }

    /// Whether the worktree side is (or index side was) an intent-to-add entry.
    #[must_use]
    pub fn is_intent_to_add(&self) -> bool {
        self.index == ChangeKind::Unmodified && self.worktree == ChangeKind::Added
    }

    /// Two-letter short status, as shown by `git status --short`.
    #[must_use]
    pub fn short_code(&self) -> String {
        match self.kind {
            StatusEntryKind::Untracked => "??".into(),
            StatusEntryKind::Ignored => "!!".into(),
            _ => [self.index.code(), self.worktree.code()].iter().collect(),
        }
    }
}

/// Result of `git status --porcelain=v2 --branch`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatusSnapshot {
    /// Branch header.
    pub branch: BranchStatus,
    /// Changed entries in porcelain order.
    pub entries: Vec<StatusEntry>,
}

/// File status in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileStatus {
    /// Content modified.
    Modified,
    /// Newly added.
    Added,
    /// Deleted.
    Deleted,
    /// Renamed.
    Renamed,
    /// Copied.
    Copied,
}

/// Diff line kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    /// Unchanged context.
    Context,
    /// Added in the new side.
    Added,
    /// Removed from the old side.
    Removed,
}

/// One line of a unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// Line kind.
    pub kind: DiffLineKind,
    /// Old-side line number.
    pub old_line: Option<u32>,
    /// New-side line number.
    pub new_line: Option<u32>,
    /// Content without the unified-diff marker or trailing newline.
    pub content: String,
    /// Whether git marked this line with `\ No newline at end of file`.
    #[serde(default)]
    pub no_newline: bool,
}

/// Unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// Old start line.
    pub old_start: u32,
    /// Old line count.
    pub old_count: u32,
    /// New start line.
    pub new_start: u32,
    /// New line count.
    pub new_count: u32,
    /// Optional function-context heading.
    pub heading: Option<String>,
    /// Lines in order.
    pub lines: Vec<DiffLine>,
}

impl DiffHunk {
    /// Whether the hunk contains added or removed lines.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        self.lines
            .iter()
            .any(|line| line.kind != DiffLineKind::Context)
    }
}

/// Parsed diff for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    /// Old path, absent for additions.
    pub old_path: Option<String>,
    /// New path, absent for deletions.
    pub new_path: Option<String>,
    /// File status.
    pub status: DiffFileStatus,
    /// Old file mode, when reported.
    pub old_mode: Option<String>,
    /// New file mode, when reported.
    pub new_mode: Option<String>,
    /// Added line count.
    pub additions: u32,
    /// Removed line count.
    pub deletions: u32,
    /// Whether git reported a binary difference.
    pub is_binary: bool,
    /// Raw extended header lines (`diff --git` through `+++`), for patch reconstruction.
    pub header: Vec<String>,
    /// Hunks in order.
    pub hunks: Vec<DiffHunk>,
}

impl DiffFile {
    /// Display path: the new path, falling back to the old path.
    #[must_use]
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

/// Lines added and removed in a file, or summed over several files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LineStats {
    /// Lines added.
    pub added: u64,
    /// Lines removed.
    pub removed: u64,
    /// Whether any binary change is included; binary files add no lines.
    pub binary: bool,
}

impl LineStats {
    /// Counts of a text change.
    #[must_use]
    pub const fn text(added: u64, removed: u64) -> Self {
        Self {
            added,
            removed,
            binary: false,
        }
    }

    /// Counts of a binary change.
    #[must_use]
    pub const fn binary() -> Self {
        Self {
            added: 0,
            removed: 0,
            binary: true,
        }
    }

    /// Both counts together: lines summed, binary when either is.
    #[must_use]
    pub const fn combined(self, other: Self) -> Self {
        Self {
            added: self.added.saturating_add(other.added),
            removed: self.removed.saturating_add(other.removed),
            binary: self.binary || other.binary,
        }
    }

    /// Whether nothing changed: no lines and no binary change.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0 && !self.binary
    }
}

impl std::iter::Sum for LineStats {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), Self::combined)
    }
}

/// A path changed by a commit or stash, from a name-status listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// New path.
    pub path: String,
    /// Old path for renames and copies.
    pub old_path: Option<String>,
    /// Change status.
    pub status: DiffFileStatus,
}

/// Address of a line within a [`DiffFile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DiffLineId {
    /// Hunk index.
    pub hunk: usize,
    /// Line index within the hunk.
    pub line: usize,
}

/// A commit from `git log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// Full object identifier.
    pub oid: String,
    /// Abbreviated object identifier.
    pub short_oid: String,
    /// Parent object identifiers.
    pub parents: Vec<String>,
    /// Author name.
    pub author_name: String,
    /// Author email.
    pub author_email: String,
    /// Author time, seconds since the Unix epoch.
    pub author_time: i64,
    /// Committer time, seconds since the Unix epoch.
    pub commit_time: i64,
    /// Ref decorations (`HEAD -> main`, `origin/main`, `tag: v1`).
    pub refs: Vec<String>,
    /// Subject line.
    pub summary: String,
}

impl Commit {
    /// Whether the commit has more than one parent.
    #[must_use]
    pub const fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// A local branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// Short name.
    pub name: String,
    /// Tip commit.
    pub oid: String,
    /// Whether this branch is checked out in the current worktree.
    pub is_head: bool,
    /// Upstream short name.
    pub upstream: Option<String>,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// Whether the configured upstream no longer exists.
    pub upstream_gone: bool,
    /// Tip committer time, seconds since the Unix epoch.
    pub commit_time: i64,
    /// Tip subject.
    pub summary: String,
    /// Worktree where the branch is checked out, if any other than the current one.
    pub worktree: Option<PathBuf>,
}

/// A remote-tracking branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBranch {
    /// Remote name.
    pub remote: String,
    /// Branch name on the remote.
    pub name: String,
    /// Tip commit.
    pub oid: String,
    /// Tip committer time, seconds since the Unix epoch.
    pub commit_time: i64,
    /// Tip subject.
    pub summary: String,
}

impl RemoteBranch {
    /// `remote/name` spelling.
    #[must_use]
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.remote, self.name)
    }
}

/// A configured remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    /// Remote name.
    pub name: String,
    /// Fetch URL.
    pub fetch_url: Option<String>,
    /// Push URL.
    pub push_url: Option<String>,
}

/// A tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    /// Tag name.
    pub name: String,
    /// Commit the tag ultimately points at.
    pub target: String,
    /// Whether the tag is annotated.
    pub annotated: bool,
    /// Tag message subject or commit subject.
    pub summary: String,
    /// Creation time, seconds since the Unix epoch.
    pub time: i64,
}

/// A stash entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashEntry {
    /// Stack index (`stash@{index}`).
    pub index: usize,
    /// Stash commit.
    pub oid: String,
    /// Stash message.
    pub message: String,
    /// Creation time, seconds since the Unix epoch.
    pub time: i64,
}

impl StashEntry {
    /// `stash@{n}` selector.
    #[must_use]
    pub fn selector(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }
}

/// A `HEAD` reflog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflogEntry {
    /// `HEAD@{n}` selector.
    pub selector: String,
    /// Commit after the operation.
    pub oid: String,
    /// Abbreviated commit.
    pub short_oid: String,
    /// Operation description.
    pub message: String,
    /// Time, seconds since the Unix epoch.
    pub time: i64,
}

/// A worktree from `git worktree list --porcelain`.
// Each flag mirrors an independent porcelain attribute.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Worktree root.
    pub path: PathBuf,
    /// Checked out commit, absent for bare entries.
    pub head: Option<String>,
    /// Checked out branch short name.
    pub branch: Option<String>,
    /// Whether this is the bare repository entry.
    pub bare: bool,
    /// Whether the worktree is detached.
    pub detached: bool,
    /// Whether the worktree is locked.
    pub locked: bool,
    /// Whether the worktree can be pruned.
    pub prunable: bool,
    /// Whether this is the main worktree.
    pub is_main: bool,
}

/// A submodule from `git submodule status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submodule {
    /// Path relative to the superproject root.
    pub path: String,
    /// Checked out commit.
    pub oid: String,
    /// State prefix.
    pub state: SubmoduleState,
    /// `git describe` output, when present.
    pub describe: Option<String>,
}

/// Submodule checkout state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmoduleState {
    /// Checked out at the recorded commit.
    Current,
    /// Not initialized.
    Uninitialized,
    /// Checked out commit differs from the recorded commit.
    Modified,
    /// Has merge conflicts.
    Conflicted,
}

/// How far a git command has got, from the progress it prints (`Writing objects:  45% (9/20)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// What it is doing (`Writing objects`).
    pub phase: String,
    /// Percent of that phase done, 0 to 100.
    pub percent: u8,
}

/// One git command run for the command log: running, or finished. Its terminal output is
/// kept by the log, by [`Self::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommandRecord {
    /// Identity within the log, increasing in start order.
    #[serde(default)]
    pub id: u64,
    /// Arguments after `git`.
    pub args: Vec<String>,
    /// Whether the command mutates repository state.
    pub mutating: bool,
    /// Whether it runs, or how it ended.
    #[serde(default)]
    pub status: CommandStatus,
    /// Wall time in milliseconds.
    pub duration_ms: u64,
    /// Why the command failed: git's message, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether output beyond the log's per-command limit was dropped.
    #[serde(default)]
    pub output_truncated: bool,
}

/// Whether a logged command runs, or how it ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Still running; the record's duration is not final.
    #[default]
    Running,
    /// Exited successfully.
    Succeeded,
    /// Failed (see the record's error).
    Failed,
}

impl GitCommandRecord {
    /// Whether the command finished successfully.
    #[must_use]
    pub fn success(&self) -> bool {
        self.status == CommandStatus::Succeeded
    }

    /// Whether the command is still running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.status == CommandStatus::Running
    }

    /// Shell-like display of the command.
    #[must_use]
    pub fn display(&self) -> String {
        let mut text = String::from("git");
        for arg in &self.args {
            text.push(' ');
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                text.push('\'');
                text.push_str(&arg.replace('\'', "'\\''"));
                text.push('\'');
            } else {
                text.push_str(arg);
            }
        }
        text
    }
}

/// An interactive rebase instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebaseAction {
    /// Use the commit.
    Pick,
    /// Use the commit but edit its message.
    Reword,
    /// Stop after applying the commit.
    Edit,
    /// Meld into the previous commit, combining messages.
    Squash,
    /// Meld into the previous commit, discarding this message.
    Fixup,
    /// Remove the commit.
    Drop,
}

impl RebaseAction {
    /// Todo-list keyword.
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Edit => "edit",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }
}

/// One line of a prepared interactive rebase todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseTodoItem {
    /// Instruction.
    pub action: RebaseAction,
    /// Commit to apply.
    pub oid: String,
    /// Subject, for display only.
    pub summary: String,
}

/// Where a prefilled commit message comes from, in default precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommitMessageSource {
    /// The `BCODE_GIT_COMMIT_MSG` environment variable (the message itself).
    Env,
    /// bcode's draft file, `BCODE_GIT_COMMIT_MSG` in the git directory.
    BcodeDraft,
    /// lazygit's unsent message, `LAZYGIT_PENDING_COMMIT` in the git directory.
    LazygitDraft,
    /// `MERGE_MSG`, written by a merge, cherry-pick or revert that stopped.
    Merge,
    /// `SQUASH_MSG`, written by `git merge --squash`.
    Squash,
    /// `COMMIT_EDITMSG`, used only when it does not match `HEAD` (a commit that did not land).
    EditMsg,
    /// The file named by `commit.template`.
    Template,
}

impl CommitMessageSource {
    /// Every source in default precedence order.
    pub const ALL: [Self; 7] = [
        Self::Env,
        Self::BcodeDraft,
        Self::LazygitDraft,
        Self::Merge,
        Self::Squash,
        Self::EditMsg,
        Self::Template,
    ];

    /// Configuration name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::BcodeDraft => "bcode-draft",
            Self::LazygitDraft => "lazygit-draft",
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::EditMsg => "edit-msg",
            Self::Template => "template",
        }
    }

    /// Where the text lives, for display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Env => "$BCODE_GIT_COMMIT_MSG",
            Self::BcodeDraft => "BCODE_GIT_COMMIT_MSG",
            Self::LazygitDraft => "LAZYGIT_PENDING_COMMIT",
            Self::Merge => "MERGE_MSG",
            Self::Squash => "SQUASH_MSG",
            Self::EditMsg => "COMMIT_EDITMSG",
            Self::Template => "commit.template",
        }
    }

    /// Source with a configuration name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|source| source.name().eq_ignore_ascii_case(name.trim()))
    }

    /// Whether the text is a draft someone wrote rather than one git generated.
    #[must_use]
    pub const fn is_draft(self) -> bool {
        matches!(self, Self::Env | Self::BcodeDraft | Self::LazygitDraft)
    }
}

/// Reset mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    /// Move `HEAD` only.
    Soft,
    /// Move `HEAD` and reset the index.
    Mixed,
    /// Move `HEAD`, reset the index and worktree.
    Hard,
    /// Like hard, but refuse to discard local changes that would be overwritten.
    Keep,
}

impl ResetMode {
    /// Command-line flag.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Soft => "--soft",
            Self::Mixed => "--mixed",
            Self::Hard => "--hard",
            Self::Keep => "--keep",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_codes_and_side_classification() {
        let entry = StatusEntry {
            path: "a".into(),
            orig_path: None,
            kind: StatusEntryKind::Ordinary,
            index: ChangeKind::Modified,
            worktree: ChangeKind::Unmodified,
            submodule: false,
        };
        assert_eq!(entry.short_code(), "M ");
        assert!(entry.has_staged());
        assert!(!entry.has_unstaged());
        let untracked = StatusEntry {
            kind: StatusEntryKind::Untracked,
            index: ChangeKind::Unmodified,
            ..entry
        };
        assert_eq!(untracked.short_code(), "??");
        assert!(!untracked.has_staged());
        assert!(untracked.has_unstaged());
    }

    #[test]
    fn command_record_display_quotes_arguments() {
        let record = GitCommandRecord {
            args: vec!["commit".into(), "-m".into(), "it's done".into()],
            mutating: true,
            status: CommandStatus::Succeeded,
            duration_ms: 1,
            error: None,
            id: 0,
            output_truncated: false,
        };
        assert_eq!(record.display(), "git commit -m 'it'\\''s done'");
    }
}
