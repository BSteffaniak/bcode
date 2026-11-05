#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Git repository access through the `git` command line.
//!
//! Every operation runs the user's own `git`, so hooks, credential helpers, signing, attributes
//! and configuration behave exactly as they do in a terminal. Output is requested in stable
//! machine-readable formats and parsed into [`bcode_git_models`] types.
//!
//! Commands are synchronous; interactive frontends should run them off their input thread.

use std::fmt;

pub mod diff;
pub mod parse;
pub mod patch;
mod repo;
mod runner;

pub use bcode_git_models as models;
pub use repo::{
    ApplyTarget, BCODE_COMMIT_DRAFT, CommitOptions, ConflictSide, DiffTarget, LAZYGIT_COMMIT_DRAFT,
    LogQuery, PushOptions, Repository, StashOptions,
};
pub use runner::{CommandLog, GitInvocation, GitOutput, GitRunner};

/// Failure running or interpreting git.
#[derive(Debug)]
pub enum GitError {
    /// git could not be spawned or communicated with.
    Io(std::io::Error),
    /// git exited unsuccessfully.
    Command {
        /// Arguments after `git`.
        args: String,
        /// Exit code, if the process exited normally.
        code: Option<i32>,
        /// Trimmed standard error.
        stderr: String,
        /// Trimmed standard output (some commands report conflicts there).
        stdout: String,
    },
    /// git output could not be parsed.
    Parse(String),
    /// The path is not inside a git worktree.
    NotARepository(std::path::PathBuf),
    /// The requested operation is not supported for this input.
    Unsupported(String),
}

impl GitError {
    /// Short user-facing message: git's reason first, then any of its hints.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Command { stderr, stdout, .. } => {
                let text = if stderr.is_empty() { stdout } else { stderr };
                let (hints, reasons): (Vec<&str>, Vec<&str>) = text
                    .lines()
                    .map(|line| line.strip_prefix("error: ").unwrap_or(line))
                    .partition(|line| line.starts_with("hint:"));
                reasons
                    .into_iter()
                    .chain(hints)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            other => other.to_string(),
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "failed to run git: {error}"),
            Self::Command {
                args, code, stderr, ..
            } => match code {
                Some(code) => write!(formatter, "git {args} exited with {code}: {stderr}"),
                None => write!(formatter, "git {args} was terminated: {stderr}"),
            },
            Self::Parse(message) => write!(formatter, "unexpected git output: {message}"),
            Self::NotARepository(path) => {
                write!(
                    formatter,
                    "{} is not inside a git repository",
                    path.display()
                )
            }
            Self::Unsupported(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GitError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::GitError;

    #[test]
    fn message_puts_the_reason_before_hints() {
        let error = GitError::Command {
            args: "merge --ff-only origin/main".into(),
            code: Some(128),
            stderr: "hint: Diverging branches can't be fast-forwarded\nhint: use rebase\n\
                     fatal: Not possible to fast-forward, aborting."
                .into(),
            stdout: String::new(),
        };
        assert_eq!(
            error.message(),
            "fatal: Not possible to fast-forward, aborting.\n\
             hint: Diverging branches can't be fast-forwarded\nhint: use rebase"
        );
    }
}
