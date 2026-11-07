//! How full status scans run.
//!
//! Most work is narrowed to changed paths and never needs a full scan; scans happen at
//! startup, on an explicit refresh, and when a change source reports it lost events.

use bcode_git::models::StatusSnapshot;
use bcode_git::{GitError, Repository, StatusScope};

/// Which [`StatusScanner`] to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScannerKind {
    /// Plain `git status`.
    #[default]
    Plain,
    /// `git status` with git's filesystem monitor and untracked cache. git saves both in the
    /// index and starts its monitor daemon for each scanned worktree. Falls back to plain
    /// scans where git cannot monitor.
    Fsmonitor,
}

/// Runs full status scans of a worktree.
pub trait StatusScanner: Send {
    /// Short name for diagnostics.
    fn name(&self) -> &'static str;

    /// Status of every path in `repo`.
    ///
    /// # Errors
    /// * [`GitError`] when git fails.
    fn full_status(&mut self, repo: &Repository) -> Result<StatusSnapshot, GitError>;
}

/// Plain `git status`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainScanner;

impl StatusScanner for PlainScanner {
    fn name(&self) -> &'static str {
        "plain"
    }

    fn full_status(&mut self, repo: &Repository) -> Result<StatusSnapshot, GitError> {
        repo.status_scoped(StatusScope::All { fsmonitor: false })
    }
}

/// `git status` through git's filesystem monitor, falling back to plain scans for good once
/// git refuses it.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsmonitorScanner {
    unavailable: bool,
}

impl StatusScanner for FsmonitorScanner {
    fn name(&self) -> &'static str {
        if self.unavailable {
            "fsmonitor (unavailable, plain)"
        } else {
            "fsmonitor"
        }
    }

    fn full_status(&mut self, repo: &Repository) -> Result<StatusSnapshot, GitError> {
        if !self.unavailable {
            match repo.status_scoped(StatusScope::All { fsmonitor: true }) {
                Ok(status) => return Ok(status),
                Err(_) => self.unavailable = true,
            }
        }
        PlainScanner.full_status(repo)
    }
}

/// A scanner of `kind`.
#[must_use]
pub fn scanner(kind: ScannerKind) -> Box<dyn StatusScanner> {
    match kind {
        ScannerKind::Plain => Box::new(PlainScanner),
        ScannerKind::Fsmonitor => Box::new(FsmonitorScanner::default()),
    }
}
