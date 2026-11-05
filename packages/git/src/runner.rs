//! `git` subprocess execution with stable, machine-parseable defaults.
//!
//! Commands shown in the command log run on a pseudo-terminal: standard error (progress,
//! hooks, advice, errors) is a terminal, so it is shown as git and hooks drew it and recorded
//! live in the [`CommandLog`], while standard output stays a byte-exact pipe for parsing.
//! Other commands run on plain pipes.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bcode_git_models::{CommandStatus, GitCommandRecord};
use bmux_pty::{PtyCommand, PtyEvent, PtySize};
use bmux_terminal_grid::{GridLimits, TerminalGridStream};

use crate::GitError;

/// Configuration forced on every invocation so output stays parseable regardless of user config.
/// Color is chosen per invocation (see [`GitRunner::run`]).
const FORCED_CONFIG: &[&str] = &[
    "-c",
    "core.quotepath=false",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicprefix=false",
    "-c",
    "log.showsignature=false",
];

/// Columns of the terminal commands in the command log run on; interpret their output at
/// this size.
pub const COMMAND_TERMINAL_COLUMNS: u16 = 160;
/// Rows of the terminal commands in the command log run on.
pub const COMMAND_TERMINAL_ROWS: u16 = 40;
const TERMINAL: PtySize = PtySize::new(COMMAND_TERMINAL_COLUMNS, COMMAND_TERMINAL_ROWS);
/// Terminal output kept per command; the rest is dropped and the record marked truncated.
const OUTPUT_LIMIT: usize = 1024 * 1024;
/// Commands whose terminal output is kept; older records keep no output.
const OUTPUT_RECORDS: usize = 100;
/// History kept when turning terminal output into an error message.
const MESSAGE_SCROLLBACK: usize = 10_000;

/// Bounded, shared record of commands for presentation in a command log: running and
/// finished commands, and the terminal output of the most recent ones.
#[derive(Debug, Clone)]
pub struct CommandLog {
    inner: Arc<Mutex<LogState>>,
    capacity: usize,
}

#[derive(Debug, Default)]
struct LogState {
    records: VecDeque<GitCommandRecord>,
    outputs: BTreeMap<u64, Vec<u8>>,
    next_id: u64,
    revision: u64,
}

impl LogState {
    fn record(&mut self, id: u64) -> Option<&mut GitCommandRecord> {
        self.records.iter_mut().rev().find(|record| record.id == id)
    }
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
            inner: Arc::new(Mutex::new(LogState {
                records: VecDeque::with_capacity(capacity.min(1024)),
                ..LogState::default()
            })),
            capacity: capacity.max(1),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LogState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record a command starting; returns its id.
    fn start(&self, mut record: GitCommandRecord) -> u64 {
        let mut state = self.state();
        let id = state.next_id;
        state.next_id += 1;
        record.id = id;
        record.status = CommandStatus::Running;
        if state.records.len() == self.capacity
            && let Some(evicted) = state.records.pop_front()
        {
            state.outputs.remove(&evicted.id);
        }
        state.records.push_back(record);
        state.revision += 1;
        id
    }

    /// Add terminal output of a command, up to the per-command limit.
    fn append(&self, id: u64, bytes: &[u8]) {
        let mut state = self.state();
        if state.record(id).is_none() {
            return;
        }
        let output = state.outputs.entry(id).or_default();
        let room = OUTPUT_LIMIT.saturating_sub(output.len());
        output.extend_from_slice(&bytes[..bytes.len().min(room)]);
        let truncated = bytes.len() > room;
        while state.outputs.len() > OUTPUT_RECORDS {
            state.outputs.pop_first();
        }
        if truncated && let Some(record) = state.record(id) {
            record.output_truncated = true;
        }
        state.revision += 1;
    }

    /// Record how a command ended.
    fn finish(&self, id: u64, finished: impl FnOnce(&mut GitCommandRecord)) {
        let mut state = self.state();
        if let Some(record) = state.record(id) {
            finished(record);
        }
        state.revision += 1;
    }

    /// Snapshot of retained records, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<GitCommandRecord> {
        self.state().records.iter().cloned().collect()
    }

    /// Terminal output of the command with `id` after its first `from` bytes: what it wrote
    /// to its terminal since that point. Empty when there is none or it is no longer kept.
    #[must_use]
    pub fn output(&self, id: u64, from: usize) -> Vec<u8> {
        self.state()
            .outputs
            .get(&id)
            .and_then(|output| output.get(from..))
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    }

    /// Changes whenever a record or output changes, to skip work when nothing did.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.state().revision
    }

    /// Number of retained records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state().records.len()
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
        let id = (invocation.log || invocation.mutating).then(|| {
            self.log.start(GitCommandRecord {
                id: 0,
                args: invocation
                    .args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
                mutating: invocation.mutating,
                status: CommandStatus::Running,
                duration_ms: 0,
                error: None,
                output_truncated: false,
            })
        });
        let result = id
            .and_then(|id| self.run_terminal(invocation, id))
            .unwrap_or_else(|| self.run_piped(invocation));
        if let Some(id) = id {
            self.log.finish(id, |record| {
                record.status = if result.is_ok() {
                    CommandStatus::Succeeded
                } else {
                    CommandStatus::Failed
                };
                record.duration_ms =
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                record.error = result.as_ref().err().map(GitError::message);
            });
        }
        result
    }

    /// Run on a pseudo-terminal, recording terminal output under `id` as it arrives. `None`
    /// when no terminal can be created, to run on pipes instead.
    fn run_terminal(
        &self,
        invocation: &GitInvocation,
        id: u64,
    ) -> Option<Result<GitOutput, GitError>> {
        let mut command = PtyCommand::new(&self.program)
            .arg("--no-pager")
            .args(FORCED_CONFIG)
            // Color only reaches the terminal; standard output is a pipe, so it stays plain.
            .args(["-c", "color.ui=auto"])
            .args(&invocation.args)
            .current_dir(&self.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_MESSAGES", "C")
            .env("LANGUAGE", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .capture_stdout();
        for (key, value) in &invocation.env {
            command = command.env(key, value);
        }
        if let Some(input) = &invocation.stdin {
            command = command.input(input.clone());
        }
        let mut process = command.spawn(TERMINAL).ok()?;
        let (mut stdout, mut terminal) = (Vec::new(), Vec::new());
        let mut exit = None;
        for event in process.events() {
            match event {
                PtyEvent::Output(bytes) => {
                    self.log.append(id, &bytes);
                    let room = OUTPUT_LIMIT.saturating_sub(terminal.len());
                    terminal.extend_from_slice(&bytes[..bytes.len().min(room)]);
                }
                PtyEvent::Stdout(bytes) => {
                    // Shown in the terminal as a terminal would show it.
                    self.log.append(id, &terminal_newlines(&bytes));
                    stdout.extend(bytes);
                }
                PtyEvent::Exited(status) => exit = Some(status),
            }
        }
        let Some(exit) = exit else {
            return Some(Err(GitError::Io(std::io::Error::other(
                "git output ended without an exit status",
            ))));
        };
        let message = terminal_text(&terminal);
        Some(if exit.success() {
            Ok(GitOutput {
                stdout,
                stderr: message.into_bytes(),
            })
        } else {
            Err(GitError::Command {
                args: invocation
                    .args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" "),
                code: exit.code(),
                stderr: message,
                stdout: String::from_utf8_lossy(&stdout).trim().to_owned(),
            })
        })
    }

    fn run_piped(&self, invocation: &GitInvocation) -> Result<GitOutput, GitError> {
        let mut command = Command::new(&self.program);
        command
            .arg("--no-pager")
            .args(FORCED_CONFIG)
            .args(["-c", "color.ui=never"])
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
        let (status, output) = spawn_and_wait(&mut command, invocation.stdin.as_deref())?;
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
    }
}

/// Newlines as a terminal shows them (`\n` moves to the start of the next line).
fn terminal_newlines(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for byte in bytes {
        if *byte == b'\n' {
            output.push(b'\r');
        }
        output.push(*byte);
    }
    output
}

/// The text a terminal shows for `bytes`: progress rewrites and colors applied, wrapped lines
/// joined.
fn terminal_text(bytes: &[u8]) -> String {
    let Ok(mut stream) = TerminalGridStream::new(
        TERMINAL.cols,
        TERMINAL.rows,
        GridLimits {
            scrollback_rows: MESSAGE_SCROLLBACK,
        },
    ) else {
        return String::from_utf8_lossy(bytes).trim().to_owned();
    };
    stream.process(bytes);
    bmux_terminal_grid::content_text(stream.grid())
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
