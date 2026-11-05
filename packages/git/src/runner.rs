//! `git` subprocess execution with stable, machine-parseable defaults.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bcode_git_models::GitCommandRecord;

use crate::GitError;

/// Configuration forced on every invocation so output stays parseable regardless of user config.
const FORCED_CONFIG: &[&str] = &[
    "-c",
    "color.ui=never",
    "-c",
    "core.quotepath=false",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicprefix=false",
    "-c",
    "log.showsignature=false",
];

/// Bounded, shared record of executed commands for presentation in a command log.
#[derive(Debug, Clone)]
pub struct CommandLog {
    inner: Arc<Mutex<VecDeque<GitCommandRecord>>>,
    capacity: usize,
}

impl Default for CommandLog {
    fn default() -> Self {
        Self::with_capacity(500)
    }
}

impl CommandLog {
    /// Log retaining at most `capacity` records.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(1024)))),
            capacity: capacity.max(1),
        }
    }

    fn push(&self, record: GitCommandRecord) {
        let mut records = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if records.len() == self.capacity {
            records.pop_front();
        }
        records.push_back(record);
    }

    /// Snapshot of retained records, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<GitCommandRecord> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Number of retained records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no records are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A single git invocation.
#[derive(Debug, Clone, Default)]
pub struct GitInvocation {
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    stdin: Option<Vec<u8>>,
    mutating: bool,
    log: bool,
}

impl GitInvocation {
    /// Read-only invocation that does not take optional locks.
    #[must_use]
    pub fn read<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            args: args
                .into_iter()
                .map(|arg| arg.as_ref().to_owned())
                .collect(),
            env: vec![("GIT_OPTIONAL_LOCKS".into(), "0".into())],
            stdin: None,
            mutating: false,
            log: false,
        }
    }

    /// Invocation that changes repository state; recorded in the command log.
    #[must_use]
    pub fn write<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            args: args
                .into_iter()
                .map(|arg| arg.as_ref().to_owned())
                .collect(),
            env: Vec::new(),
            stdin: None,
            mutating: true,
            log: true,
        }
    }

    /// Provide standard input.
    #[must_use]
    pub fn stdin(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    /// Set an environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Record this invocation in the command log even when read-only.
    #[must_use]
    pub const fn logged(mut self) -> Self {
        self.log = true;
        self
    }
}

/// Captured process output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    /// Standard output bytes.
    pub stdout: Vec<u8>,
    /// Standard error bytes.
    pub stderr: Vec<u8>,
}

impl GitOutput {
    /// Standard output as UTF-8, replacing invalid sequences.
    #[must_use]
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Runs `git` in one working directory.
#[derive(Debug, Clone)]
pub struct GitRunner {
    cwd: PathBuf,
    program: OsString,
    log: CommandLog,
}

impl GitRunner {
    /// Runner using the `git` found on `PATH`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, log: CommandLog) -> Self {
        Self {
            cwd: cwd.into(),
            program: "git".into(),
            log,
        }
    }

    /// Use a specific git executable.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Shared command log.
    #[must_use]
    pub const fn log(&self) -> &CommandLog {
        &self.log
    }

    /// Run a read-only command and return standard output as lossy UTF-8.
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be spawned.
    /// * [`GitError::Command`] when git exits unsuccessfully.
    pub fn read<I, S>(&self, args: I) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(&GitInvocation::read(args))
            .map(|output| output.stdout_lossy())
    }

    /// Run a mutating command and return standard output as lossy UTF-8.
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be spawned.
    /// * [`GitError::Command`] when git exits unsuccessfully.
    pub fn write<I, S>(&self, args: I) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run(&GitInvocation::write(args))
            .map(|output| output.stdout_lossy())
    }

    /// Run an invocation.
    ///
    /// # Errors
    /// * [`GitError::Io`] when git cannot be spawned or its input cannot be written.
    /// * [`GitError::Command`] when git exits unsuccessfully.
    pub fn run(&self, invocation: &GitInvocation) -> Result<GitOutput, GitError> {
        let started = Instant::now();
        let mut command = Command::new(&self.program);
        command
            .arg("--no-pager")
            .args(FORCED_CONFIG)
            .args(&invocation.args)
            .current_dir(&self.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_MESSAGES", "C")
            .env("LANGUAGE", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &invocation.env {
            command.env(key, value);
        }
        let result = spawn_and_wait(&mut command, invocation.stdin.as_deref()).and_then(
            |(status, output)| {
                if status.success() {
                    Ok(output)
                } else {
                    Err(GitError::Command {
                        args: invocation
                            .args
                            .iter()
                            .map(|arg| arg.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join(" "),
                        code: status.code(),
                        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                    })
                }
            },
        );
        if invocation.log || invocation.mutating {
            self.log.push(GitCommandRecord {
                args: invocation
                    .args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
                mutating: invocation.mutating,
                success: result.is_ok(),
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                error: result.as_ref().err().map(GitError::message),
            });
        }
        result
    }
}

fn spawn_and_wait(
    command: &mut Command,
    stdin: Option<&[u8]>,
) -> Result<(std::process::ExitStatus, GitOutput), GitError> {
    let mut child = command.spawn()?;
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        // Write on a separate thread so large patches cannot deadlock against full output pipes.
        let input = input.to_vec();
        let writer = std::thread::spawn(move || pipe.write_all(&input));
        let output = child.wait_with_output()?;
        writer
            .join()
            .map_err(|_| GitError::Io(std::io::Error::other("stdin writer panicked")))??;
        return Ok((
            output.status,
            GitOutput {
                stdout: output.stdout,
                stderr: output.stderr,
            },
        ));
    }
    let output = child.wait_with_output()?;
    Ok((
        output.status,
        GitOutput {
            stdout: output.stdout,
            stderr: output.stderr,
        },
    ))
}
