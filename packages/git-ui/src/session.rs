//! Everything needed to reopen the client as it was left.
//!
//! The session is saved per launch directory next to the panel layout (see
//! [`crate::panels`]). Only transient things are not kept: messages, open menus and prompts,
//! and operations in flight.

use std::collections::BTreeMap;

use bcode_git::models::Commit;
use serde::{Deserialize, Serialize};

use crate::app::View;
use crate::config::ScreenMode;

/// Saved session state. Windows are named by their tabs and views by their ids, so the
/// state survives reordering the configured windows; anything that no longer exists is
/// skipped on restore.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Screen mode, including a maximized panel (`full`).
    pub screen_mode: Option<ScreenMode>,
    /// The side window with focus, or the one the main view belongs to.
    pub side: Option<String>,
    /// Whether the main view had focus.
    pub main_focused: bool,
    /// Selected tab of each side window: window name to view id.
    pub tabs: BTreeMap<String, String>,
    /// Selected row of each list, by its stable key: view id to key.
    pub selected: BTreeMap<String, String>,
    /// Selected row index of each list, used when its key is gone.
    pub cursors: BTreeMap<String, usize>,
    /// Active list filters: view id to text.
    pub filters: BTreeMap<String, String>,
    /// Files as a tree rather than a flat list.
    pub file_tree: Option<bool>,
    /// Collapsed directories of the file tree.
    pub collapsed_dirs: Vec<String>,
    /// Whether the command log is shown.
    pub command_log: Option<bool>,
    /// Lines of diff context.
    pub diff_context: Option<u32>,
    /// Whether the commit graph is shown.
    pub show_graph: Option<bool>,
    /// Ref compared against in diffing mode.
    pub diff_base: Option<String>,
    /// Remote whose branches are listed.
    pub remote: Option<String>,
    /// Commits of a ref, shown in a side window.
    pub sub_commits: Option<SavedSubCommits>,
    /// Files of a commit or stash, shown in a side window.
    pub commit_files: Option<SavedCommitFiles>,
    /// Main view position and staging.
    pub main: Option<SavedMain>,
    /// Commits copied for cherry-picking.
    pub cherry: Vec<Commit>,
    /// Undo history, oldest first.
    pub undo: Vec<SavedJournal>,
    /// Redo history, oldest first.
    pub redo: Vec<SavedJournal>,
    /// Shell commands run from the `:` prompt, oldest first.
    pub shell_history: Vec<String>,
}

/// Commits of a ref in a side window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSubCommits {
    /// Window name.
    pub window: String,
    /// Revision listed.
    pub revision: String,
    /// Title shown.
    pub title: String,
}

/// Files of a commit or stash in a side window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCommitFiles {
    /// Window name.
    pub window: String,
    /// What the files belong to.
    pub target: SavedTarget,
    /// Revision (commit id or stash selector).
    pub revision: String,
    /// Title shown.
    pub title: String,
}

/// A diff target that can be listed again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SavedTarget {
    /// A commit.
    Commit(String),
    /// A stash entry.
    Stash(String),
    /// Two revisions.
    Range {
        /// Old side.
        from: String,
        /// New side.
        to: String,
    },
}

/// Main view position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMain {
    /// Section with focus.
    pub section: usize,
    /// Row of each section.
    pub cursor: [usize; 2],
    /// Staging selection, when staging lines.
    pub staging: Option<SavedStaging>,
}

/// Line staging state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedStaging {
    /// Range anchor row.
    pub anchor: Option<usize>,
    /// Whole-hunk selection.
    pub hunk_mode: bool,
}

/// An undo-journal entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedJournal {
    /// What was done.
    pub label: String,
    /// `HEAD` before: branch and commit.
    pub before: (Option<String>, String),
    /// `HEAD` after: branch and commit.
    pub after: (Option<String>, String),
}

/// Stable id of a view in saved sessions.
#[must_use]
pub const fn view_id(view: View) -> &'static str {
    match view {
        View::Status => "status",
        View::Files => "files",
        View::Worktrees => "worktrees",
        View::Submodules => "submodules",
        View::Branches => "branches",
        View::Remotes => "remotes",
        View::RemoteBranches => "remote-branches",
        View::Tags => "tags",
        View::SubCommits => "sub-commits",
        View::Commits => "commits",
        View::Reflog => "reflog",
        View::Stash => "stash",
        View::CommitFiles => "commit-files",
    }
}

/// The view with a saved id.
#[must_use]
pub fn view_from_id(id: &str) -> Option<View> {
    View::ALL.into_iter().find(|view| view_id(*view) == id)
}
