#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Neovim RPC backed Vim editing for Bcode.
//!
//! This crate owns reusable Vim edit behavior. It starts isolated headless
//! Neovim processes, drives them through RPC, records state after each edit
//! step, and optionally writes the final buffer back to the requested file.
//! Diff rendering intentionally reuses `bcode_tui_components::diff_viewer`, so
//! this crate does not need a dedicated diff dependency. Neovim is controlled
//! through the embedded msgpack-RPC transport provided by `nvim --embed`.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_tui_components::diff_viewer::{DiffLineKind, diff_from_text};
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio as nvim_create;
use nvim_rs::rpc::handler::Dummy;
use nvim_rs::{Neovim, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::process::{Child, ChildStdin, Command};
use tokio::runtime::Builder;
use tokio::task::JoinHandle;
use tokio::time;

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_CONTEXT_RADIUS: usize = 3;
const NVIM_EXECUTABLE: &str = "nvim";
const NVIM_MODE_KEY: &str = "mode";
const NVIM_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_millis(100);

/// Granularity for live Vim edit observations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VimEditObservationGranularity {
    /// Observe after each requested step.
    #[default]
    Step,
    /// Observe after each key token inside key steps.
    Key,
}

/// Request to run Vim edit steps against a single file.
#[derive(Debug, Clone)]
pub struct VimEditRequest {
    /// File to edit.
    pub path: PathBuf,
    /// Optional Neovim executable override.
    pub nvim_executable: Option<PathBuf>,
    /// Ordered Vim edit steps.
    pub steps: Vec<VimEditStep>,
    /// Preview or apply behavior.
    pub mode: VimEditMode,
    /// Sandbox policy used while executing steps.
    pub sandbox: VimEditSandbox,
    /// Timeout for the full operation.
    pub timeout: Duration,
    /// Live observation granularity.
    pub observation_granularity: VimEditObservationGranularity,
}

/// Whether to preview edits or write the final buffer back to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VimEditMode {
    /// Return a diff without changing the requested file.
    Preview,
    /// Write the final edited buffer back to the requested file.
    Apply,
}

/// Sandbox policy for Neovim command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VimEditSandbox {
    /// Default safer mode that blocks obvious external/file-escape commands.
    Default,
    /// Explicit dangerous bypass for users who intentionally opt out of checks.
    DangerouslyDisabled,
}

/// One Vim edit step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VimEditStep {
    /// Send real Vim key notation such as `/foo<CR>` or `ciw`.
    Keys { input: String },
    /// Insert literal text at the current Neovim cursor/mode.
    Insert { text: String },
    /// Execute an Ex command through Neovim RPC.
    Ex { command: String },
}

/// Result of a Vim edit operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditResult {
    /// Whether the final buffer differs from the original file text.
    pub changed: bool,
    /// Unified-ish textual diff generated from existing Bcode diff infrastructure.
    pub diff: String,
    /// Final cursor position.
    pub cursor: CursorPosition,
    /// Final Neovim mode.
    pub nvim_mode: String,
    /// Final context window around the cursor.
    pub final_context: TextContext,
    /// Stepwise observations captured after executing each step.
    pub events: Vec<VimEditEvent>,
}

/// One-indexed cursor position reported to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CursorPosition {
    /// One-indexed line number.
    pub line: usize,
    /// One-indexed column number.
    pub column: usize,
}

/// Bounded line context around a cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextContext {
    /// One-indexed line number corresponding to `lines[0]`.
    pub start_line: usize,
    /// Context lines.
    pub lines: Vec<String>,
}

/// Result for one edited file in a multi-file Vim edit request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditMultiFileResult {
    /// Edited file path.
    pub path: PathBuf,
    /// Whether this file changed.
    pub changed: bool,
    /// File-specific diff.
    pub diff: String,
    /// Final cursor position after the last ordered entry for this file.
    pub cursor: CursorPosition,
    /// Final Neovim mode after the last ordered entry for this file.
    pub nvim_mode: String,
    /// Final bounded context after the last ordered entry for this file.
    pub final_context: TextContext,
    /// Events recorded while this file's entries were active.
    pub events: Vec<VimEditEvent>,
}

/// One ordered file entry inside a multi-file Vim edit operation.
#[derive(Debug, Clone)]
pub struct VimEditMultiFileEntry {
    /// File to switch to for this entry.
    pub path: PathBuf,
    /// Steps to run after switching to this file.
    pub steps: Vec<VimEditStep>,
}

/// Explicit ordered multi-file Vim edit request.
#[derive(Debug, Clone)]
pub struct VimEditMultiFileRequest {
    /// Ordered file operations. Entries execute strictly in array order; repeated
    /// paths are allowed and reuse the same Neovim buffer/temp file so registers
    /// and other Neovim state persist across file switches.
    pub files: Vec<VimEditMultiFileEntry>,
    /// Optional Neovim executable override.
    pub nvim_executable: Option<PathBuf>,
    /// Whether to preview or apply after every ordered operation succeeds.
    pub mode: VimEditMode,
    /// Sandbox policy for every step.
    pub sandbox: VimEditSandbox,
    /// Timeout for the whole ordered workflow.
    pub timeout: Duration,
    /// Live observation granularity.
    pub observation_granularity: VimEditObservationGranularity,
}

/// Result of a multi-file Vim edit request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditMultiFileEditResult {
    /// Per-file results.
    pub files: Vec<VimEditMultiFileResult>,
    /// Combined diff containing every changed file.
    pub diff: String,
    /// Whether any file changed.
    pub changed: bool,
}

struct VimEditExecution {
    result: VimEditResult,
    final_text: String,
}

struct MultiFileExecution {
    files: Vec<MultiFileExecutionFile>,
}

struct MultiFileExecutionFile {
    path: PathBuf,
    original: String,
    final_text: String,
    cursor: CursorPosition,
    nvim_mode: String,
    final_context: TextContext,
    events: Vec<VimEditEvent>,
}

impl MultiFileExecution {
    fn into_result(self) -> VimEditMultiFileEditResult {
        let files = self
            .files
            .into_iter()
            .map(|file| {
                let changed = file.original != file.final_text;
                VimEditMultiFileResult {
                    diff: render_diff(&file.path, &file.original, &file.final_text),
                    path: file.path,
                    changed,
                    cursor: file.cursor,
                    nvim_mode: file.nvim_mode,
                    final_context: file.final_context,
                    events: file.events,
                }
            })
            .collect::<Vec<_>>();
        let mut diff = String::new();
        for file in &files {
            if file.changed {
                diff.push_str(&file.diff);
            }
        }
        let changed = files.iter().any(|file| file.changed);
        VimEditMultiFileEditResult {
            files,
            diff,
            changed,
        }
    }
}

/// Snapshot for an interactive Vim edit session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditSessionSnapshot {
    /// Cursor at snapshot time.
    pub cursor: CursorPosition,
    /// Neovim mode at snapshot time.
    pub nvim_mode: String,
    /// Bounded context around the cursor.
    pub context: TextContext,
    /// Diff between original file text and current session buffer.
    pub diff: String,
    /// Whether the session buffer differs from the original text.
    pub changed: bool,
}

/// Result of applying one step to an interactive Vim edit session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditSessionInputResult {
    /// Event captured for the applied step.
    pub event: VimEditEvent,
    /// Snapshot after the step.
    pub snapshot: VimEditSessionSnapshot,
}

/// Result of finishing an interactive Vim edit session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditSessionFinishResult {
    /// Whether the final buffer differs from the original file text.
    pub changed: bool,
    /// Final diff.
    pub diff: String,
    /// Final cursor position.
    pub cursor: CursorPosition,
    /// Final Neovim mode.
    pub nvim_mode: String,
    /// Final bounded context around the cursor.
    pub final_context: TextContext,
    /// Stepwise observations captured by the session.
    pub events: Vec<VimEditEvent>,
    /// Whether the final text was written to the requested file.
    pub applied: bool,
}

/// Long-lived RPC-backed Vim edit session.
pub struct VimEditSession {
    runtime: tokio::runtime::Runtime,
    session: Option<NeovimSession>,
    _temp_file: NamedTempFile,
    path: PathBuf,
    original: String,
    previous_buffer: String,
    sandbox: VimEditSandbox,
    timeout: Duration,
    events: Vec<VimEditEvent>,
    next_step_index: usize,
    started_at: Instant,
    last_accessed_at: Instant,
}

/// State captured for one Vim edit step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditEvent {
    /// Zero-indexed step number.
    pub step_index: usize,
    /// Executed step.
    pub step: VimEditStep,
    /// Cursor before this step.
    pub before_cursor: CursorPosition,
    /// Cursor after this step.
    pub after_cursor: CursorPosition,
    /// Neovim mode after this step.
    pub nvim_mode: String,
    /// Context after this step.
    pub context: TextContext,
    /// Whether the buffer changed compared with the previous observation.
    pub changed: bool,
    /// Optional step message.
    pub message: Option<String>,
}

/// Compact live frame emitted while Vim edit execution progresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditFrame {
    /// Current file index in the request.
    pub file_index: usize,
    /// Total file entries in the request.
    pub file_total: usize,
    /// Current edited file path.
    pub path: PathBuf,
    /// Zero-indexed global step number.
    pub step_index: usize,
    /// Total ordered steps.
    pub step_total: usize,
    /// Step that produced this frame.
    pub step: VimEditStep,
    /// Zero-indexed key-token substep within a key step.
    pub substep_index: Option<usize>,
    /// Total key-token substeps within a key step.
    pub substep_total: Option<usize>,
    /// Key token that produced this frame.
    pub input_token: Option<String>,
    /// Cursor before this step.
    pub before_cursor: CursorPosition,
    /// Cursor after this step.
    pub after_cursor: CursorPosition,
    /// Neovim mode after this step.
    pub nvim_mode: String,
    /// Bounded context after this step.
    pub context: TextContext,
    /// Whether this step changed the buffer.
    pub changed: bool,
    /// Optional status message.
    pub message: Option<String>,
}

/// Sink for compact live Vim edit frames.
pub type VimEditFrameSink<'a> = &'a mut (dyn FnMut(VimEditFrame) + Send);

/// State captured when one Vim edit step fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VimEditFailureState {
    /// Zero-indexed failing step number.
    pub step_index: usize,
    /// Cursor at failure, when Neovim is still observable.
    pub cursor: Option<CursorPosition>,
    /// Neovim mode at failure, when Neovim is still observable.
    pub nvim_mode: Option<String>,
    /// Bounded context at failure, when Neovim is still observable.
    pub context: Option<TextContext>,
}

/// Error returned while running a Vim edit operation.
#[derive(Debug, Error)]
pub enum VimEditError {
    /// Vim key notation was malformed.
    #[error("invalid Vim key notation at byte {index} in {input:?}: {message}")]
    InvalidKeyNotation {
        /// Full input.
        input: String,
        /// Byte index of the malformed token.
        index: usize,
        /// Human-readable error.
        message: String,
    },
    /// The requested file could not be inspected.
    #[error("failed to inspect {path:?}: {source}")]
    Metadata { path: PathBuf, source: io::Error },
    /// The requested file could not be read.
    #[error("failed to read {path:?}: {source}")]
    ReadFile { path: PathBuf, source: io::Error },
    /// The requested file is too large for MVP Vim editing.
    #[error("file {path:?} is {bytes} bytes, which exceeds the {max_bytes} byte limit")]
    FileTooLarge {
        /// File path.
        path: PathBuf,
        /// Observed byte size.
        bytes: u64,
        /// Maximum supported byte size.
        max_bytes: u64,
    },
    /// The requested file is not UTF-8 text.
    #[error("file {path:?} is not valid UTF-8 text")]
    NonUtf8 { path: PathBuf },
    /// A temporary edit file could not be created or written.
    #[error("failed to create temporary edit file: {0}")]
    TempFile(io::Error),
    /// Neovim could not be started.
    #[error("failed to start `{executable}`: {source}")]
    StartNeovim {
        /// Neovim executable.
        executable: String,
        /// Source error.
        source: io::Error,
    },
    /// Neovim RPC failed.
    #[error("Neovim RPC error: {0}")]
    Rpc(String),
    /// Neovim exited unexpectedly before or during RPC editing.
    #[error("Neovim exited unexpectedly: {0}")]
    UnexpectedExit(String),
    /// A Vim edit step failed after Neovim reported an error.
    #[error("step {state_step} failed: {message}", state_step = state.step_index)]
    StepFailed {
        /// Observable state at the failing step.
        state: VimEditFailureState,
        /// Model-readable failure message.
        message: String,
    },
    /// A Vim edit step was rejected by sandbox policy.
    #[error("step {step_index} rejected by sandbox: {reason}")]
    UnsafeCommand {
        /// Rejected step index.
        step_index: usize,
        /// Rejection reason.
        reason: String,
    },
    /// Neovim did not complete its embedded RPC startup handshake in time.
    #[error("Neovim startup timed out after {timeout_ms} ms")]
    StartupTimeout { timeout_ms: u128 },
    /// The operation timed out.
    #[error("Vim edit operation timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u128 },
    /// Failed to terminate and reap the Neovim child process.
    #[error("failed to terminate Neovim child: {source}")]
    Shutdown { source: io::Error },
    /// Failed to write the final buffer back to the requested path.
    #[error("failed to write {path:?}: {source}")]
    WriteFile { path: PathBuf, source: io::Error },
}

/// Run explicit ordered multi-file Vim edit steps.
///
/// Apply mode is all-or-nothing with respect to Bcode writes: every declared
/// file is edited against a temporary copy in one shared Neovim process first,
/// and changed requested files are written only after the full ordered workflow
/// succeeds.
///
/// # Errors
///
/// Returns an error if any file validation, sandbox check, Neovim step, timeout,
/// or final write fails.
pub fn run_vim_multi_file_edit(
    request: &VimEditMultiFileRequest,
) -> Result<VimEditMultiFileEditResult, VimEditError> {
    run_vim_multi_file_edit_observed(request, None)
}

/// Run multi-file Vim edit steps and emit compact live frames after each completed step.
///
/// # Errors
///
/// Returns the same errors as [`run_vim_multi_file_edit`].
pub fn run_vim_multi_file_edit_observed(
    request: &VimEditMultiFileRequest,
    observer: Option<VimEditFrameSink<'_>>,
) -> Result<VimEditMultiFileEditResult, VimEditError> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| VimEditError::StartNeovim {
            executable: "tokio runtime".to_string(),
            source,
        })?
        .block_on(run_vim_multi_file_edit_inner(request, observer))
}

async fn run_vim_multi_file_edit_inner(
    request: &VimEditMultiFileRequest,
    observer: Option<VimEditFrameSink<'_>>,
) -> Result<VimEditMultiFileEditResult, VimEditError> {
    let execution = run_vim_multi_file_edit_prepare(request, observer).await?;
    if request.mode == VimEditMode::Apply {
        for file in &execution.files {
            if file.original != file.final_text {
                fs::write(&file.path, file.final_text.as_bytes()).map_err(|source| {
                    VimEditError::WriteFile {
                        path: file.path.clone(),
                        source,
                    }
                })?;
            }
        }
    }
    Ok(execution.into_result())
}

/// Start a long-lived interactive Vim edit session.
///
/// # Errors
///
/// Returns an error for the same file validation, Neovim startup, timeout, and
/// RPC failures as [`run_vim_edit`].
pub fn start_vim_edit_session(request: VimEditRequest) -> Result<VimEditSession, VimEditError> {
    VimEditSession::start(request)
}

impl VimEditSession {
    /// Start a long-lived interactive Vim edit session.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, the file is unsupported,
    /// Neovim cannot be started, or the initial session snapshot fails.
    pub fn start(request: VimEditRequest) -> Result<Self, VimEditError> {
        let original = read_text_file(&request.path)?;
        let temp_file = temp_file_with_contents(&original)?;
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|source| VimEditError::StartNeovim {
                executable: "tokio runtime".to_string(),
                source,
            })?;
        let executable = request
            .nvim_executable
            .as_deref()
            .unwrap_or_else(|| Path::new(NVIM_EXECUTABLE));
        let session = runtime.block_on(start_neovim(executable))?;
        runtime.block_on(timeout_result(request.timeout, async {
            session.configure_isolation().await?;
            session.edit_path(temp_file.path()).await?;
            session.buffer_text().await
        }))?;
        let previous_buffer = original.clone();
        let now = Instant::now();
        Ok(Self {
            runtime,
            session: Some(session),
            _temp_file: temp_file,
            path: request.path,
            original,
            previous_buffer,
            sandbox: request.sandbox,
            timeout: request.timeout,
            events: Vec::with_capacity(request.steps.len()),
            next_step_index: 0,
            started_at: now,
            last_accessed_at: now,
        })
    }

    /// Apply one step to the session.
    ///
    /// # Errors
    ///
    /// Returns an error when sandbox policy rejects the step, Neovim reports a
    /// step failure, the session times out, or the session has already ended.
    pub fn input(&mut self, step: VimEditStep) -> Result<VimEditSessionInputResult, VimEditError> {
        self.touch();
        let step_index = self.next_step_index;
        let timeout = self.timeout;
        let sandbox = self.sandbox;
        let previous_buffer = self.previous_buffer.clone();
        let result = {
            let session = self.session_ref()?;
            self.runtime.block_on(timeout_result(timeout, async {
                apply_session_step(
                    session,
                    step,
                    step_index,
                    sandbox,
                    previous_buffer,
                    &self.original,
                    &self.path,
                )
                .await
            }))
        };
        let (event, next_buffer, snapshot) = match result {
            Ok(result) => result,
            Err(error) => {
                if matches!(
                    error,
                    VimEditError::Timeout { .. } | VimEditError::UnexpectedExit(_)
                ) {
                    let _ = self.shutdown_session();
                }
                return Err(error);
            }
        };
        self.previous_buffer = next_buffer;
        self.next_step_index = self.next_step_index.saturating_add(1);
        self.events.push(event.clone());
        Ok(VimEditSessionInputResult { event, snapshot })
    }

    /// Return current session state.
    ///
    /// # Errors
    ///
    /// Returns an error when the session has ended, timed out, or cannot query Neovim.
    pub fn snapshot(&mut self) -> Result<VimEditSessionSnapshot, VimEditError> {
        self.touch();
        let timeout = self.timeout;
        let original = self.original.clone();
        let path = self.path.clone();
        let result = {
            let session = self.session_ref()?;
            self.runtime.block_on(timeout_result(timeout, async {
                session_snapshot(session, &original, &path).await
            }))
        };
        match result {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                if matches!(
                    error,
                    VimEditError::Timeout { .. } | VimEditError::UnexpectedExit(_)
                ) {
                    let _ = self.shutdown_session();
                }
                Err(error)
            }
        }
    }

    /// Finish the session and optionally apply the final buffer to the requested file.
    ///
    /// # Errors
    ///
    /// Returns an error when the final state cannot be read or the requested file
    /// cannot be written in apply mode.
    pub fn finish(mut self, apply: bool) -> Result<VimEditSessionFinishResult, VimEditError> {
        self.touch();
        let timeout = self.timeout;
        let original = self.original.clone();
        let path = self.path.clone();
        let result = {
            let session = self.session_ref()?;
            self.runtime.block_on(timeout_result(timeout, async {
                let final_text = session.buffer_text().await?;
                let cursor = session.cursor().await?;
                let nvim_mode = session.mode().await?;
                let final_context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
                let diff = render_diff(&path, &original, &final_text);
                Ok::<_, VimEditError>((final_text, cursor, nvim_mode, final_context, diff))
            }))
        };
        let (final_text, cursor, nvim_mode, final_context, diff) = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = self.shutdown_session();
                return Err(error);
            }
        };
        self.shutdown_session()?;
        let changed = original != final_text;
        if apply && changed {
            fs::write(&path, final_text.as_bytes()).map_err(|source| VimEditError::WriteFile {
                path: path.clone(),
                source,
            })?;
        }
        Ok(VimEditSessionFinishResult {
            changed,
            diff,
            cursor,
            nvim_mode,
            final_context,
            events: std::mem::take(&mut self.events),
            applied: apply && changed,
        })
    }

    /// Cancel the session without writing to the requested file.
    pub fn cancel(mut self) {
        let _ = self.shutdown_session();
    }

    /// Return when the session started.
    #[must_use]
    pub const fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Return when the session was last accessed.
    #[must_use]
    pub const fn last_accessed_at(&self) -> Instant {
        self.last_accessed_at
    }

    fn touch(&mut self) {
        self.last_accessed_at = Instant::now();
    }

    fn session_ref(&self) -> Result<&NeovimSession, VimEditError> {
        self.session
            .as_ref()
            .ok_or_else(|| VimEditError::UnexpectedExit("session is already closed".to_string()))
    }

    fn shutdown_session(&mut self) -> Result<(), VimEditError> {
        if let Some(session) = self.session.take() {
            self.runtime
                .block_on(session.shutdown())
                .map_err(|source| VimEditError::Shutdown { source })?;
        }
        Ok(())
    }
}

impl Drop for VimEditSession {
    fn drop(&mut self) {
        let _ = self.shutdown_session();
    }
}

async fn start_neovim(executable: &Path) -> Result<NeovimSession, VimEditError> {
    time::timeout(NVIM_STARTUP_TIMEOUT, NeovimSession::start(executable))
        .await
        .unwrap_or(Err(VimEditError::StartupTimeout {
            timeout_ms: NVIM_STARTUP_TIMEOUT.as_millis(),
        }))
}

async fn timeout_result<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = Result<T, VimEditError>>,
) -> Result<T, VimEditError> {
    time::timeout(timeout, future)
        .await
        .unwrap_or(Err(VimEditError::Timeout {
            timeout_ms: timeout.as_millis(),
        }))
}

async fn apply_session_step(
    session: &NeovimSession,
    step: VimEditStep,
    step_index: usize,
    sandbox: VimEditSandbox,
    previous_buffer: String,
    original: &str,
    path: &Path,
) -> Result<(VimEditEvent, String, VimEditSessionSnapshot), VimEditError> {
    let before_cursor = session.cursor().await?;
    if let Err(error) = reject_unsafe_step(&step, step_index, sandbox) {
        let state = session.failure_state(step_index).await;
        return Err(VimEditError::StepFailed {
            state,
            message: error.to_string(),
        });
    }
    if let Err(error) = session.apply_step(&step).await {
        let state = session.failure_state(step_index).await;
        let message = classify_step_error(&step, &error.to_string());
        return Err(VimEditError::StepFailed { state, message });
    }
    let after_cursor = session.cursor().await?;
    let nvim_mode = session.mode().await?;
    let context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
    let next_buffer = session.buffer_text().await?;
    let changed = next_buffer != previous_buffer;
    let event = VimEditEvent {
        step_index,
        step,
        before_cursor,
        after_cursor,
        nvim_mode: nvim_mode.clone(),
        context: context.clone(),
        changed,
        message: Some("step completed successfully".to_string()),
    };
    let snapshot = VimEditSessionSnapshot {
        cursor: after_cursor,
        nvim_mode,
        context,
        diff: render_diff(path, original, &next_buffer),
        changed: original != next_buffer,
    };
    Ok((event, next_buffer, snapshot))
}

async fn session_snapshot(
    session: &NeovimSession,
    original: &str,
    path: &Path,
) -> Result<VimEditSessionSnapshot, VimEditError> {
    let current_text = session.buffer_text().await?;
    Ok(VimEditSessionSnapshot {
        cursor: session.cursor().await?,
        nvim_mode: session.mode().await?,
        context: session.context(DEFAULT_CONTEXT_RADIUS).await?,
        diff: render_diff(path, original, &current_text),
        changed: original != current_text,
    })
}

/// Run Vim edit steps against a single file.
///
/// # Errors
///
/// Returns an error if:
///
/// * the requested file cannot be read or written
/// * the file is too large or not UTF-8 text
/// * Neovim cannot be started or controlled through RPC
/// * default sandbox mode rejects a step
/// * the operation times out
pub fn run_vim_edit(request: VimEditRequest) -> Result<VimEditResult, VimEditError> {
    run_vim_edit_observed(request, None)
}

/// Run Vim edit steps and emit compact live frames after each completed step.
///
/// # Errors
///
/// Returns the same errors as [`run_vim_edit`].
pub fn run_vim_edit_observed(
    request: VimEditRequest,
    observer: Option<VimEditFrameSink<'_>>,
) -> Result<VimEditResult, VimEditError> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| VimEditError::StartNeovim {
            executable: "tokio runtime".to_string(),
            source,
        })?
        .block_on(run_vim_edit_inner(request, observer))
}

async fn run_vim_edit_inner(
    request: VimEditRequest,
    observer: Option<VimEditFrameSink<'_>>,
) -> Result<VimEditResult, VimEditError> {
    let execution = run_vim_edit_prepare(&request, observer).await?;
    if request.mode == VimEditMode::Apply && execution.result.changed {
        fs::write(&request.path, execution.final_text.as_bytes()).map_err(|source| {
            VimEditError::WriteFile {
                path: request.path.clone(),
                source,
            }
        })?;
    }
    Ok(execution.result)
}

async fn run_vim_multi_file_edit_prepare(
    request: &VimEditMultiFileRequest,
    observer: Option<VimEditFrameSink<'_>>,
) -> Result<MultiFileExecution, VimEditError> {
    let mut files = BTreeMap::<PathBuf, (String, NamedTempFile)>::new();
    for entry in &request.files {
        if !files.contains_key(&entry.path) {
            let original = read_text_file(&entry.path)?;
            let temp_file = temp_file_with_contents(&original)?;
            files.insert(entry.path.clone(), (original, temp_file));
        }
    }

    let session = start_neovim(
        request
            .nvim_executable
            .as_deref()
            .unwrap_or_else(|| Path::new(NVIM_EXECUTABLE)),
    )
    .await?;
    let operation = run_vim_multi_file_session(&session, request, &files, observer);
    let session_result = time::timeout(request.timeout, operation)
        .await
        .unwrap_or(Err(VimEditError::Timeout {
            timeout_ms: request.timeout.as_millis(),
        }));
    let shutdown_result = session
        .shutdown()
        .await
        .map_err(|source| VimEditError::Shutdown { source });
    shutdown_result?;
    session_result
}

#[allow(clippy::too_many_lines)]
async fn run_vim_multi_file_session(
    session: &NeovimSession,
    request: &VimEditMultiFileRequest,
    files: &BTreeMap<PathBuf, (String, NamedTempFile)>,
    mut observer: Option<VimEditFrameSink<'_>>,
) -> Result<MultiFileExecution, VimEditError> {
    session.configure_isolation().await?;
    let mut previous_buffers = BTreeMap::<PathBuf, String>::new();
    let mut events_by_path = BTreeMap::<PathBuf, Vec<VimEditEvent>>::new();
    let mut step_index = 0usize;
    let step_total = request
        .files
        .iter()
        .map(|entry| entry.steps.len())
        .sum::<usize>();

    for (file_index, entry) in request.files.iter().enumerate() {
        let Some((_, temp_file)) = files.get(&entry.path) else {
            return Err(VimEditError::ReadFile {
                path: entry.path.clone(),
                source: io::Error::new(io::ErrorKind::NotFound, "undeclared multi-file entry"),
            });
        };
        session.edit_path(temp_file.path()).await?;
        let mut previous_buffer = previous_buffers
            .remove(&entry.path)
            .unwrap_or(session.buffer_text().await?);
        for step in &entry.steps {
            let before_cursor = session.cursor().await?;
            if let Err(error) = reject_unsafe_step(step, step_index, request.sandbox) {
                let state = session.failure_state(step_index).await;
                return Err(VimEditError::StepFailed {
                    state,
                    message: error.to_string(),
                });
            }
            let emitted_key_frames = request.observation_granularity
                == VimEditObservationGranularity::Key
                && observer.is_some()
                && key_step_is_safe_to_split(step);
            let step_result = if emitted_key_frames {
                observe_key_step(KeyStepObservation {
                    session,
                    step,
                    step_index,
                    step_total,
                    file_index,
                    file_total: request.files.len(),
                    path: &entry.path,
                    previous_buffer: &mut previous_buffer,
                    observer: &mut observer,
                })
                .await
            } else {
                session.apply_step(step).await
            };
            if let Err(error) = step_result {
                let state = session.failure_state(step_index).await;
                let message = classify_step_error(step, &error.to_string());
                return Err(VimEditError::StepFailed { state, message });
            }
            let after_cursor = session.cursor().await?;
            let nvim_mode = session.mode().await?;
            let context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
            let next_buffer = session.buffer_text().await?;
            let changed = next_buffer != previous_buffer;
            previous_buffer = next_buffer;
            let message = Some("step completed successfully".to_string());
            let event = VimEditEvent {
                step_index,
                step: step.clone(),
                before_cursor,
                after_cursor,
                nvim_mode,
                context,
                changed,
                message,
            };
            if !emitted_key_frames && let Some(observer) = observer.as_deref_mut() {
                observer(VimEditFrame {
                    file_index,
                    file_total: request.files.len(),
                    path: entry.path.clone(),
                    step_index,
                    step_total,
                    step: event.step.clone(),
                    substep_index: None,
                    substep_total: None,
                    input_token: None,
                    before_cursor: event.before_cursor,
                    after_cursor: event.after_cursor,
                    nvim_mode: event.nvim_mode.clone(),
                    context: event.context.clone(),
                    changed: event.changed,
                    message: event.message.clone(),
                });
            }
            events_by_path
                .entry(entry.path.clone())
                .or_default()
                .push(event);
            step_index = step_index.saturating_add(1);
        }
        previous_buffers.insert(entry.path.clone(), previous_buffer);
    }

    let mut output_files = Vec::with_capacity(files.len());
    for (path, (original, temp_file)) in files {
        session.edit_path(temp_file.path()).await?;
        let final_text = previous_buffers
            .get(path)
            .cloned()
            .unwrap_or_else(|| original.clone());
        let cursor = session.cursor().await?;
        let nvim_mode = session.mode().await?;
        let final_context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
        output_files.push(MultiFileExecutionFile {
            path: path.clone(),
            original: original.clone(),
            final_text,
            cursor,
            nvim_mode,
            final_context,
            events: events_by_path.remove(path).unwrap_or_default(),
        });
    }
    Ok(MultiFileExecution {
        files: output_files,
    })
}

async fn run_vim_edit_prepare(
    request: &VimEditRequest,
    observer: Option<VimEditFrameSink<'_>>,
) -> Result<VimEditExecution, VimEditError> {
    let original = read_text_file(&request.path)?;
    let temp_file = temp_file_with_contents(&original)?;
    let session = start_neovim(
        request
            .nvim_executable
            .as_deref()
            .unwrap_or_else(|| Path::new(NVIM_EXECUTABLE)),
    )
    .await?;
    let operation = run_vim_edit_session(&session, request, temp_file.path(), observer);
    let session_result = time::timeout(request.timeout, operation)
        .await
        .unwrap_or(Err(VimEditError::Timeout {
            timeout_ms: request.timeout.as_millis(),
        }));
    let shutdown_result = session
        .shutdown()
        .await
        .map_err(|source| VimEditError::Shutdown { source });
    shutdown_result?;
    let (final_text, cursor, nvim_mode, final_context, events) = session_result?;

    let changed = original != final_text;
    Ok(VimEditExecution {
        result: VimEditResult {
            changed,
            diff: render_diff(&request.path, &original, &final_text),
            cursor,
            nvim_mode,
            final_context,
            events,
        },
        final_text,
    })
}

type SessionEditOutput = (
    String,
    CursorPosition,
    String,
    TextContext,
    Vec<VimEditEvent>,
);

async fn run_vim_edit_session(
    session: &NeovimSession,
    request: &VimEditRequest,
    temp_path: &Path,
    mut observer: Option<VimEditFrameSink<'_>>,
) -> Result<SessionEditOutput, VimEditError> {
    session.configure_isolation().await?;
    session.edit_path(temp_path).await?;

    let mut previous_buffer = session.buffer_text().await?;
    let mut events = Vec::with_capacity(request.steps.len());
    for (step_index, step) in request.steps.iter().enumerate() {
        let before_cursor = session.cursor().await?;
        if let Err(error) = reject_unsafe_step(step, step_index, request.sandbox) {
            let state = session.failure_state(step_index).await;
            return Err(VimEditError::StepFailed {
                state,
                message: error.to_string(),
            });
        }
        let emitted_key_frames = request.observation_granularity
            == VimEditObservationGranularity::Key
            && observer.is_some()
            && key_step_is_safe_to_split(step);
        let step_result = if emitted_key_frames {
            observe_key_step(KeyStepObservation {
                session,
                step,
                step_index,
                step_total: request.steps.len(),
                file_index: 0,
                file_total: 1,
                path: &request.path,
                previous_buffer: &mut previous_buffer,
                observer: &mut observer,
            })
            .await
        } else {
            session.apply_step(step).await
        };
        if let Err(error) = step_result {
            let state = session.failure_state(step_index).await;
            let message = classify_step_error(step, &error.to_string());
            return Err(VimEditError::StepFailed { state, message });
        }
        let after_cursor = session.cursor().await?;
        let nvim_mode = session.mode().await?;
        let context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
        let next_buffer = session.buffer_text().await?;
        let changed = next_buffer != previous_buffer;
        previous_buffer = next_buffer;
        let message = Some("step completed successfully".to_string());
        let event = VimEditEvent {
            step_index,
            step: step.clone(),
            before_cursor,
            after_cursor,
            nvim_mode,
            context,
            changed,
            message,
        };
        if !emitted_key_frames && let Some(observer) = observer.as_deref_mut() {
            observer(VimEditFrame {
                file_index: 0,
                file_total: 1,
                path: request.path.clone(),
                step_index,
                step_total: request.steps.len(),
                step: event.step.clone(),
                substep_index: None,
                substep_total: None,
                input_token: None,
                before_cursor: event.before_cursor,
                after_cursor: event.after_cursor,
                nvim_mode: event.nvim_mode.clone(),
                context: event.context.clone(),
                changed: event.changed,
                message: event.message.clone(),
            });
        }
        events.push(event);
    }

    let final_text = previous_buffer;
    let cursor = session.cursor().await?;
    let nvim_mode = session.mode().await?;
    let final_context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
    Ok((final_text, cursor, nvim_mode, final_context, events))
}

fn key_step_is_safe_to_split(step: &VimEditStep) -> bool {
    let VimEditStep::Keys { input } = step else {
        return false;
    };
    let Ok(tokens) = tokenize_vim_keys(input) else {
        return true;
    };
    !tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "c" | "d" | "y" | "p" | "P" | "r" | "R" | "g" | "z" | "<C-v>" | "v" | "V"
        )
    })
}

struct KeyStepObservation<'a, 'b> {
    session: &'a NeovimSession,
    step: &'a VimEditStep,
    step_index: usize,
    step_total: usize,
    file_index: usize,
    file_total: usize,
    path: &'a Path,
    previous_buffer: &'a mut String,
    observer: &'a mut Option<VimEditFrameSink<'b>>,
}

async fn observe_key_step(context: KeyStepObservation<'_, '_>) -> Result<(), VimEditError> {
    let KeyStepObservation {
        session,
        step,
        step_index,
        step_total,
        file_index,
        file_total,
        path,
        previous_buffer,
        observer,
    } = context;
    let VimEditStep::Keys { input } = step else {
        return session.apply_step(step).await;
    };
    let tokens = tokenize_vim_keys(input)?;
    session.clear_vim_error().await?;
    let substep_total = tokens.len();
    for (substep_index, token) in tokens.iter().enumerate() {
        let before_cursor = session.cursor().await?;
        session.input_key_token(token).await?;
        let after_cursor = session.cursor().await?;
        let nvim_mode = session.mode().await?;
        let context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
        let next_buffer = session.buffer_text().await?;
        let changed = next_buffer != *previous_buffer;
        *previous_buffer = next_buffer;
        if let Some(observer) = observer.as_deref_mut() {
            observer(VimEditFrame {
                file_index,
                file_total,
                path: path.to_path_buf(),
                step_index,
                step_total,
                step: step.clone(),
                substep_index: Some(substep_index),
                substep_total: Some(substep_total),
                input_token: Some(token.clone()),
                before_cursor,
                after_cursor,
                nvim_mode,
                context,
                changed,
                message: Some("key token completed successfully".to_string()),
            });
        }
    }
    session.fail_on_vim_error().await
}

fn tokenize_vim_keys(input: &str) -> Result<Vec<String>, VimEditError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices();
    while let Some((index, ch)) = chars.next() {
        if ch != '<' {
            tokens.push(ch.to_string());
            continue;
        }
        let Some((end, _)) = chars.by_ref().find(|(_, candidate)| *candidate == '>') else {
            return Err(VimEditError::InvalidKeyNotation {
                input: input.to_string(),
                index,
                message: "unclosed angle key token".to_string(),
            });
        };
        tokens.push(input[index..=end].to_string());
    }
    Ok(tokens)
}

fn read_text_file(path: &Path) -> Result<String, VimEditError> {
    let metadata = fs::metadata(path).map_err(|source| VimEditError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(VimEditError::FileTooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
            max_bytes: MAX_FILE_BYTES,
        });
    }
    let bytes = fs::read(path).map_err(|source| VimEditError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    String::from_utf8(bytes).map_err(|_| VimEditError::NonUtf8 {
        path: path.to_path_buf(),
    })
}

fn temp_file_with_contents(contents: &str) -> Result<NamedTempFile, VimEditError> {
    let file = NamedTempFile::new().map_err(VimEditError::TempFile)?;
    fs::write(file.path(), contents).map_err(VimEditError::TempFile)?;
    Ok(file)
}

struct ProcessTermination {
    pid: Option<u32>,
    graceful: bool,
    forced: bool,
    exit_status: ExitStatus,
}

impl ProcessTermination {
    fn verified(self) -> Self {
        debug_assert!(!(self.graceful && self.forced));
        let _ = (self.pid, self.exit_status);
        self
    }
}

async fn wait_for_child_exit(
    child: &mut Child,
    timeout: Duration,
) -> io::Result<Option<ExitStatus>> {
    time::timeout(timeout, child.wait())
        .await
        .map_or(Ok(None), |status| status.map(Some))
}

async fn terminate_child(
    child: &mut Child,
    graceful_requested: bool,
) -> io::Result<ProcessTermination> {
    let pid = child.id();
    if let Some(exit_status) = wait_for_child_exit(child, CHILD_EXIT_TIMEOUT).await? {
        return Ok(ProcessTermination {
            pid,
            graceful: graceful_requested,
            forced: false,
            exit_status,
        }
        .verified());
    }

    child.start_kill()?;
    let exit_status = wait_for_child_exit(child, CHILD_EXIT_TIMEOUT)
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("child process {pid:?} did not exit after forced termination"),
            )
        })?;
    Ok(ProcessTermination {
        pid,
        graceful: false,
        forced: true,
        exit_status,
    }
    .verified())
}

struct NeovimSession {
    nvim: Neovim<Compat<ChildStdin>>,
    io_handle: JoinHandle<Result<(), Box<nvim_rs::error::LoopError>>>,
    child: Child,
}

impl NeovimSession {
    async fn start(executable: &Path) -> Result<Self, VimEditError> {
        let mut command = Command::new(executable);
        command
            .arg("--headless")
            .arg("--clean")
            .arg("-n")
            .arg("--noplugin")
            .arg("--embed")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (nvim, io_handle, child) =
            nvim_create::new_child_cmd(&mut command, Dummy::<Compat<ChildStdin>>::new())
                .await
                .map_err(|source| VimEditError::StartNeovim {
                    executable: display_from_current_dir(executable).to_string(),
                    source,
                })?;
        Ok(Self {
            nvim,
            io_handle,
            child,
        })
    }

    async fn configure_isolation(&self) -> Result<(), VimEditError> {
        for command in [
            // These options are supported by Neovim and intentionally set after
            // startup to make the embedded RPC edit process deterministic.
            "set nomodeline",
            "set noexrc",
            "set noswapfile",
            "set hidden",
            "set encoding=utf-8",
            "set fileencoding=utf-8",
        ] {
            self.nvim_command(command).await?;
        }
        Ok(())
    }

    async fn edit_path(&self, path: &Path) -> Result<(), VimEditError> {
        let _ = self.nvim_command("setlocal nomodified").await;
        let path_string = path.display().to_string();
        let escaped = self
            .nvim
            .call_function("fnameescape", vec![Value::from(path_string)])
            .await
            .map_err(rpc_call_error)?
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| VimEditError::Rpc("fnameescape did not return a string".to_string()))?;
        self.nvim_command(&format!("edit {escaped}")).await
    }

    async fn apply_step(&self, step: &VimEditStep) -> Result<(), VimEditError> {
        self.clear_vim_error().await?;
        match step {
            VimEditStep::Keys { input } => self.apply_key_input(input).await?,
            VimEditStep::Insert { text } => {
                self.nvim
                    .paste(text, false, -1)
                    .await
                    .map(|_| ())
                    .map_err(rpc_call_error)?;
            }
            VimEditStep::Ex { command } => self.nvim_command(command).await?,
        }
        self.fail_on_vim_error().await
    }

    async fn apply_key_input(&self, input: &str) -> Result<(), VimEditError> {
        let keys = self
            .nvim
            .replace_termcodes(input, true, true, true)
            .await
            .map_err(rpc_call_error)?;
        self.nvim
            .feedkeys(&keys, "x", false)
            .await
            .map_err(rpc_call_error)
    }

    async fn input_key_token(&self, input: &str) -> Result<(), VimEditError> {
        self.apply_key_input(input).await
    }

    async fn clear_vim_error(&self) -> Result<(), VimEditError> {
        self.nvim
            .command("let v:errmsg = ''")
            .await
            .map_err(rpc_call_error)
    }

    async fn fail_on_vim_error(&self) -> Result<(), VimEditError> {
        let error = self.nvim.eval("v:errmsg").await.map_err(rpc_call_error)?;
        let Some(message) = value_to_string(&error) else {
            return Ok(());
        };
        if message.trim().is_empty() {
            Ok(())
        } else {
            Err(VimEditError::Rpc(message))
        }
    }

    async fn nvim_command(&self, command: &str) -> Result<(), VimEditError> {
        self.nvim.command(command).await.map_err(rpc_call_error)
    }

    async fn buffer_text(&self) -> Result<String, VimEditError> {
        let buffer = self.nvim.get_current_buf().await.map_err(rpc_call_error)?;
        let lines = buffer
            .get_lines(0, -1, false)
            .await
            .map_err(rpc_call_error)?;
        let mut text = lines.join("\n");
        let endofline = self
            .nvim
            .get_option_value("endofline", Vec::new())
            .await
            .map_err(rpc_call_error)
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        if endofline {
            text.push('\n');
        }
        Ok(text)
    }

    async fn cursor(&self) -> Result<CursorPosition, VimEditError> {
        let window = self.nvim.get_current_win().await.map_err(rpc_call_error)?;
        let (line, column) = window.get_cursor().await.map_err(rpc_call_error)?;
        Ok(CursorPosition {
            line: nonnegative_i64_to_usize(line).saturating_add(0),
            column: nonnegative_i64_to_usize(column).saturating_add(1),
        })
    }

    async fn mode(&self) -> Result<String, VimEditError> {
        let values = self.nvim.get_mode().await.map_err(rpc_call_error)?;
        values
            .into_iter()
            .find_map(|(key, value)| {
                (value_to_string(&key).as_deref() == Some(NVIM_MODE_KEY))
                    .then(|| value_to_string(&value))
                    .flatten()
            })
            .ok_or_else(|| VimEditError::Rpc("Neovim did not return current mode".to_string()))
    }

    async fn context(&self, radius: usize) -> Result<TextContext, VimEditError> {
        let cursor = self.cursor().await?;
        let buffer = self.nvim.get_current_buf().await.map_err(rpc_call_error)?;
        let start_line = cursor.line.saturating_sub(radius).max(1);
        let end_line = cursor.line.saturating_add(radius);
        let lines = buffer
            .get_lines(
                usize_to_i64(start_line.saturating_sub(1)),
                usize_to_i64(end_line),
                false,
            )
            .await
            .map_err(rpc_call_error)?;
        Ok(TextContext { start_line, lines })
    }

    async fn failure_state(&self, step_index: usize) -> VimEditFailureState {
        let cursor = self.cursor().await.ok();
        let nvim_mode = self.mode().await.ok();
        let context = self.context(DEFAULT_CONTEXT_RADIUS).await.ok();
        VimEditFailureState {
            step_index,
            cursor,
            nvim_mode,
            context,
        }
    }

    async fn shutdown(mut self) -> io::Result<ProcessTermination> {
        let graceful = matches!(
            time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, self.nvim.command("qa!")).await,
            Ok(Ok(()))
        );
        let termination = terminate_child(&mut self.child, graceful).await;
        if time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, &mut self.io_handle)
            .await
            .is_err()
        {
            self.io_handle.abort();
        }
        termination
    }
}

fn rpc_call_error(error: impl std::fmt::Display) -> VimEditError {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("channel closed") || lower.contains("broken pipe") || lower.contains("eof") {
        VimEditError::UnexpectedExit(message)
    } else {
        VimEditError::Rpc(message)
    }
}

fn classify_step_error(step: &VimEditStep, raw_error: &str) -> String {
    let lower = raw_error.to_ascii_lowercase();
    match step {
        VimEditStep::Keys { input } if lower.contains("pattern not found") => {
            format!(
                "search pattern in key input `{input}` was not found; adjust the search text or move the cursor before retrying"
            )
        }
        VimEditStep::Keys { input } if lower.contains("invalid") || lower.contains("key") => {
            format!(
                "key input `{input}` failed; verify the Vim key notation and retry: {raw_error}"
            )
        }
        VimEditStep::Ex { command } => {
            format!(
                "Ex command `{command}` failed in Neovim; adjust or split the command before retrying: {raw_error}"
            )
        }
        VimEditStep::Insert { .. } => {
            format!("literal insert failed in Neovim; check buffer state and retry: {raw_error}")
        }
        VimEditStep::Keys { input } => {
            format!(
                "key input `{input}` failed in Neovim; inspect returned cursor/mode/context before retrying: {raw_error}"
            )
        }
    }
}

fn reject_unsafe_step(
    step: &VimEditStep,
    step_index: usize,
    sandbox: VimEditSandbox,
) -> Result<(), VimEditError> {
    if sandbox == VimEditSandbox::DangerouslyDisabled {
        return Ok(());
    }

    match step {
        VimEditStep::Ex { command } => reject_unsafe_ex(command, step_index),
        VimEditStep::Keys { input } => reject_unsafe_keys(input, step_index),
        VimEditStep::Insert { .. } => Ok(()),
    }
}

fn reject_unsafe_ex(command: &str, step_index: usize) -> Result<(), VimEditError> {
    let command_name = sandbox_command_name(command);
    if command_name
        .as_deref()
        .is_some_and(|name| SAFE_EX_COMMANDS.contains(&name))
    {
        return Ok(());
    }

    let display_name = command_name.unwrap_or_else(|| "<empty>".to_string());
    Err(VimEditError::UnsafeCommand {
        step_index,
        reason: format!("Ex command `{display_name}` is not allowlisted in default sandbox mode"),
    })
}

fn reject_unsafe_keys(input: &str, step_index: usize) -> Result<(), VimEditError> {
    for command in command_line_key_segments(input) {
        reject_unsafe_ex(&command, step_index)?;
    }
    Ok(())
}

fn command_line_key_segments(input: &str) -> Vec<String> {
    let lower = input.to_ascii_lowercase();
    let mut segments = Vec::new();
    let mut offset = 0;
    while let Some(relative_start) = lower[offset..].find(':') {
        let start = offset.saturating_add(relative_start).saturating_add(1);
        let rest = &lower[start..];
        let end = [rest.find("<cr>"), rest.find('\n'), rest.find('\r')]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
        segments.push(rest[..end].to_string());
        offset = start.saturating_add(end);
    }
    segments
}

fn sandbox_command_name(command: &str) -> Option<String> {
    let mut rest = command.trim_start();
    rest = rest.strip_prefix(':').unwrap_or(rest).trim_start();
    loop {
        rest = strip_ex_range_prefix(rest).trim_start();
        if rest.is_empty() {
            return None;
        }
        if rest.starts_with('!') {
            return Some("!".to_string());
        }
        let name_end = rest
            .find(|character: char| !character.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        if name_end == 0 {
            return None;
        }
        let name = rest[..name_end].to_ascii_lowercase();
        if EX_COMMAND_MODIFIERS.contains(&name.as_str()) {
            rest = &rest[name_end..];
            continue;
        }
        return Some(name);
    }
}

fn strip_ex_range_prefix(command: &str) -> &str {
    command.trim_start_matches(|character: char| {
        character.is_ascii_digit()
            || matches!(
                character,
                '%' | '.' | '$' | '\'' | '<' | '>' | ',' | ';' | '+' | '-' | '/' | '?' | '*'
            )
    })
}

const SAFE_EX_COMMANDS: &[&str] = &["s", "substitute", "nohlsearch"];

const EX_COMMAND_MODIFIERS: &[&str] = &[
    "aboveleft",
    "abo",
    "belowright",
    "bel",
    "botright",
    "bo",
    "browse",
    "confirm",
    "conf",
    "hide",
    "hid",
    "keepalt",
    "keepjumps",
    "keeppatterns",
    "leftabove",
    "lefta",
    "lockmarks",
    "noautocmd",
    "noa",
    "rightbelow",
    "rightb",
    "silent",
    "sil",
    "tab",
    "topleft",
    "to",
    "vertical",
    "vert",
];

fn render_diff(path: &Path, old_text: &str, new_text: &str) -> String {
    let document = diff_from_text(
        &display_from_current_dir(path).to_string(),
        old_text,
        new_text,
    );
    let mut rendered = String::new();
    for line in document.lines {
        let prefix = match line.kind {
            DiffLineKind::FileHeader | DiffLineKind::HunkHeader => "",
            DiffLineKind::Context => " ",
            DiffLineKind::Added => "+",
            DiffLineKind::Removed => "-",
        };
        rendered.push_str(prefix);
        rendered.push_str(&line.content);
        rendered.push('\n');
    }
    rendered
}

fn value_to_string(value: &Value) -> Option<String> {
    value.as_str().map(ToOwned::to_owned)
}

fn nonnegative_i64_to_usize(value: i64) -> usize {
    usize::try_from(value.max(0)).unwrap_or(usize::MAX)
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_vim_keys_splits_plain_chars() {
        assert_eq!(tokenize_vim_keys("ciw").expect("tokens"), ["c", "i", "w"]);
    }

    #[test]
    fn tokenize_vim_keys_preserves_angle_tokens() {
        assert_eq!(
            tokenize_vim_keys("/target<CR>").expect("tokens"),
            ["/", "t", "a", "r", "g", "e", "t", "<CR>"]
        );
    }

    #[test]
    fn tokenize_vim_keys_rejects_unclosed_angle_token() {
        let error = tokenize_vim_keys("foo<Esc").expect_err("invalid token");
        assert!(error.to_string().contains("unclosed angle key token"));
    }

    #[test]
    fn interactive_session_snapshot_input_finish_apply_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo bar baz").expect("write original");
        let mut session = start_vim_edit_session(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("start session");
        let initial = session.snapshot().expect("initial snapshot");
        assert_eq!(initial.cursor.line, 1);
        assert_eq!(initial.nvim_mode, "n");
        assert_eq!(initial.context.start_line, 1);
        assert!(!initial.changed);

        session
            .input(VimEditStep::Keys {
                input: "w".to_string(),
            })
            .expect("move word");
        session
            .input(VimEditStep::Keys {
                input: "ciw".to_string(),
            })
            .expect("change word");
        let input = session
            .input(VimEditStep::Insert {
                text: "qux".to_string(),
            })
            .expect("insert word");
        assert_eq!(input.event.step_index, 2);
        assert!(input.snapshot.changed);
        session
            .input(VimEditStep::Keys {
                input: "<Esc>".to_string(),
            })
            .expect("escape");

        let finish = session.finish(true).expect("finish apply");
        assert!(finish.changed);
        assert!(finish.applied);
        assert_eq!(finish.events.len(), 4);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read edited"),
            "foo qux baz"
        );
    }

    #[test]
    fn interactive_session_finish_without_apply_leaves_file_unchanged_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let mut session = start_vim_edit_session(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("start session");
        session
            .input(VimEditStep::Ex {
                command: "%s/foo/bar/".to_string(),
            })
            .expect("substitute");
        let finish = session.finish(false).expect("finish without apply");
        assert!(finish.changed);
        assert!(!finish.applied);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo"
        );
    }

    #[test]
    fn interactive_session_input_returns_clear_step_error_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let mut session = start_vim_edit_session(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("start session");
        let error = session
            .input(VimEditStep::Keys {
                input: "/missing<CR>".to_string(),
            })
            .expect_err("missing search should fail");
        let VimEditError::StepFailed { state, message } = error else {
            panic!("expected step failure");
        };
        assert_eq!(state.step_index, 0);
        assert!(state.cursor.is_some());
        assert!(message.contains("search pattern"));
    }

    #[test]
    fn interactive_session_cancel_leaves_file_unchanged_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let mut session = start_vim_edit_session(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("start session");
        session
            .input(VimEditStep::Ex {
                command: "%s/foo/bar/".to_string(),
            })
            .expect("substitute");
        session.cancel();
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo"
        );
    }

    #[test]
    fn interactive_session_sandbox_is_fixed_for_life_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let mut session = start_vim_edit_session(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("start session");
        let error = session
            .input(VimEditStep::Ex {
                command: "write /tmp/bcode-vim-edit-session-escape".to_string(),
            })
            .expect_err("write should remain blocked");
        assert!(matches!(error, VimEditError::StepFailed { .. }));
        session.cancel();
    }

    #[test]
    fn interactive_session_start_missing_nvim_returns_clear_error() {
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let Err(error) = start_vim_edit_session(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: Some(PathBuf::from("definitely-missing-session-nvim")),
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(1),
            observation_granularity: VimEditObservationGranularity::Step,
        }) else {
            panic!("missing nvim should fail");
        };
        assert!(matches!(error, VimEditError::StartNeovim { .. }));
    }

    #[test]
    fn default_sandbox_rejects_unsafe_ex_commands() {
        let result = reject_unsafe_step(
            &VimEditStep::Ex {
                command: "write /tmp/other".to_string(),
            },
            0,
            VimEditSandbox::Default,
        );
        assert!(matches!(result, Err(VimEditError::UnsafeCommand { .. })));
    }

    #[test]
    fn default_sandbox_rejects_all_blocked_ex_families() {
        for command in [
            "!echo unsafe",
            "read /tmp/file",
            "r /tmp/file",
            "write /tmp/file",
            "w /tmp/file",
            "edit /tmp/file",
            "e /tmp/file",
            "source /tmp/file",
            "so /tmp/file",
            "lua print('unsafe')",
            "python print('unsafe')",
            "python3 print('unsafe')",
            "perl print 'unsafe'",
            "ruby puts 'unsafe'",
            "terminal",
            "term",
            "make",
            "grep unsafe *",
            "vimgrep unsafe *",
            "cexpr system('unsafe')",
            "cgetexpr system('unsafe')",
        ] {
            let result = reject_unsafe_step(
                &VimEditStep::Ex {
                    command: command.to_string(),
                },
                0,
                VimEditSandbox::Default,
            );
            assert!(
                matches!(result, Err(VimEditError::UnsafeCommand { .. })),
                "expected `{command}` to be rejected"
            );
        }
    }

    #[test]
    fn default_sandbox_allows_substitution_ex_commands() {
        for command in ["s/foo/bar/", "%s/foo/bar/g", "1,3s/foo/bar/g"] {
            let result = reject_unsafe_step(
                &VimEditStep::Ex {
                    command: command.to_string(),
                },
                0,
                VimEditSandbox::Default,
            );
            assert!(result.is_ok(), "expected `{command}` to be allowed");
        }
    }

    #[test]
    fn default_sandbox_rejects_unsafe_command_line_key_segments() {
        let result = reject_unsafe_step(
            &VimEditStep::Keys {
                input: ":write /tmp/other<CR>".to_string(),
            },
            0,
            VimEditSandbox::Default,
        );
        assert!(matches!(result, Err(VimEditError::UnsafeCommand { .. })));
    }

    #[test]
    fn default_sandbox_allows_search_key_segments() {
        let result = reject_unsafe_step(
            &VimEditStep::Keys {
                input: "/needle<CR>ciw".to_string(),
            },
            0,
            VimEditSandbox::Default,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn dangerous_sandbox_allows_unsafe_key_segments() {
        let result = reject_unsafe_step(
            &VimEditStep::Keys {
                input: ":write /tmp/other<CR>".to_string(),
            },
            0,
            VimEditSandbox::DangerouslyDisabled,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn dangerous_sandbox_allows_unsafe_ex_commands() {
        let result = reject_unsafe_step(
            &VimEditStep::Ex {
                command: "write /tmp/other".to_string(),
            },
            0,
            VimEditSandbox::DangerouslyDisabled,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn failed_step_returns_recovery_state_and_stops_later_steps() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo bar").expect("write original");
        let error = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![
                VimEditStep::Keys {
                    input: "/missing-pattern<CR>".to_string(),
                },
                VimEditStep::Ex {
                    command: "%s/foo/SHOULD_NOT_RUN/".to_string(),
                },
            ],
            mode: VimEditMode::Apply,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect_err("missing search pattern should fail");
        let VimEditError::StepFailed { state, message } = error else {
            panic!("expected step failure");
        };
        assert_eq!(state.step_index, 0);
        assert!(state.cursor.is_some());
        assert!(state.nvim_mode.is_some());
        assert!(state.context.is_some());
        assert!(message.contains("search pattern"));
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo bar"
        );
    }

    #[test]
    fn successful_events_include_recovery_observations_and_messages() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo bar").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![VimEditStep::Ex {
                command: "%s/foo/baz/".to_string(),
            }],
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("run vim edit");
        let event = result.events.first().expect("event captured");
        assert_eq!(event.step_index, 0);
        assert_eq!(event.before_cursor.line, 1);
        assert_eq!(event.after_cursor.line, 1);
        assert!(!event.nvim_mode.is_empty());
        assert_eq!(event.context.start_line, 1);
        assert!(!event.context.lines.is_empty());
        assert!(event.changed);
        assert_eq!(
            event.message.as_deref(),
            Some("step completed successfully")
        );
    }

    #[test]
    fn invalid_ex_command_reports_unsupported_command_clearly() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let error = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![VimEditStep::Ex {
                command: "definitelynotavimcommand".to_string(),
            }],
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::DangerouslyDisabled,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect_err("invalid Ex command should fail");
        let VimEditError::StepFailed { message, .. } = error else {
            panic!("expected step failure");
        };
        assert!(message.contains("Ex command `definitelynotavimcommand` failed"));
        assert!(message.contains("Not an editor command") || message.contains("E492"));
    }

    #[test]
    fn timeout_error_is_clear() {
        let error = VimEditError::Timeout { timeout_ms: 25 }.to_string();
        assert!(error.contains("timed out"));
        assert!(error.contains("25"));
    }

    #[test]
    fn file_size_and_encoding_errors_are_clear() {
        let too_large = VimEditError::FileTooLarge {
            path: PathBuf::from("large.txt"),
            bytes: MAX_FILE_BYTES + 1,
            max_bytes: MAX_FILE_BYTES,
        }
        .to_string();
        assert!(too_large.contains("large.txt"));
        assert!(too_large.contains("exceeds"));

        let non_utf8 = VimEditError::NonUtf8 {
            path: PathBuf::from("binary.bin"),
        }
        .to_string();
        assert!(non_utf8.contains("binary.bin"));
        assert!(non_utf8.contains("UTF-8"));
    }

    #[test]
    fn unexpected_exit_error_is_clear() {
        let error = VimEditError::UnexpectedExit("channel closed".to_string()).to_string();
        assert!(error.contains("unexpectedly"));
        assert!(error.contains("channel closed"));
    }

    #[test]
    fn invalid_key_notation_message_is_actionable() {
        let message = classify_step_error(
            &VimEditStep::Keys {
                input: "<DefinitelyNotAKey>".to_string(),
            },
            "Invalid key notation",
        );
        assert!(message.contains("key input `<DefinitelyNotAKey>` failed"));
        assert!(message.contains("verify the Vim key notation"));
    }

    #[test]
    fn default_sandbox_rejects_write_to_other_path_without_creating_file() {
        let other_path =
            std::env::temp_dir().join(format!("bcode-vim-edit-other-{}", std::process::id()));
        let _ = fs::remove_file(&other_path);
        let command = format!("write {}", other_path.display());
        let result = reject_unsafe_step(&VimEditStep::Ex { command }, 0, VimEditSandbox::Default);
        assert!(matches!(result, Err(VimEditError::UnsafeCommand { .. })));
        assert!(!other_path.exists());
    }

    #[test]
    fn default_sandbox_rejects_edit_escape_command() {
        let result = reject_unsafe_step(
            &VimEditStep::Ex {
                command: "edit /tmp/other".to_string(),
            },
            0,
            VimEditSandbox::Default,
        );
        assert!(matches!(result, Err(VimEditError::UnsafeCommand { .. })));
    }

    #[test]
    fn default_sandbox_rejects_shell_escape_command() {
        let result = reject_unsafe_step(
            &VimEditStep::Ex {
                command: "!echo unsafe".to_string(),
            },
            0,
            VimEditSandbox::Default,
        );
        assert!(matches!(result, Err(VimEditError::UnsafeCommand { .. })));
    }

    #[test]
    fn dangerous_bypass_is_not_default_for_requests() {
        let request = VimEditRequest {
            path: PathBuf::from("example.txt"),
            nvim_executable: None,
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(1),
            observation_granularity: VimEditObservationGranularity::Step,
        };
        assert_eq!(request.sandbox, VimEditSandbox::Default);
    }

    #[test]
    fn large_file_is_rejected_before_spawning_nvim() {
        let file = NamedTempFile::new().expect("temp file");
        file.as_file()
            .set_len(MAX_FILE_BYTES.saturating_add(1))
            .expect("extend file");
        let error = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: Some(PathBuf::from("definitely-missing-bcode-nvim")),
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(1),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect_err("large file should fail before nvim spawn");
        assert!(matches!(error, VimEditError::FileTooLarge { .. }));
    }

    #[test]
    fn non_utf8_file_is_rejected_before_spawning_nvim() {
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), [0xff, 0xfe, 0xfd]).expect("write non-utf8");
        let error = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: Some(PathBuf::from("definitely-missing-bcode-nvim")),
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(1),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect_err("non-utf8 file should fail before nvim spawn");
        assert!(matches!(error, VimEditError::NonUtf8 { .. }));
    }

    #[test]
    fn dangerous_bypass_preview_still_does_not_write_real_file_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![VimEditStep::Ex {
                command: "s/foo/bar/".to_string(),
            }],
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::DangerouslyDisabled,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("run vim edit");
        assert!(result.changed);
        assert!(!result.diff.is_empty());
        assert!(!result.events.is_empty());
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo"
        );
    }

    #[test]
    fn modeline_does_not_execute_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "# vim: set nomodifiable:\nfoo bar").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![VimEditStep::Ex {
                command: "%s/foo/baz/".to_string(),
            }],
            mode: VimEditMode::Apply,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("modeline should not make buffer unmodifiable");
        assert!(result.changed);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read edited"),
            "# vim: set nomodifiable:\nbaz bar"
        );
    }

    #[test]
    fn render_diff_uses_existing_diff_document() {
        let diff = render_diff(Path::new("sample.txt"), "foo\n", "bar\n");
        assert!(diff.contains("sample.txt"));
        assert!(diff.contains("-foo"));
        assert!(diff.contains("+bar"));
    }

    #[test]
    fn missing_nvim_returns_clear_error() {
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let error = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: Some(PathBuf::from("definitely-missing-bcode-nvim")),
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(1),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect_err("missing nvim should error");
        assert!(matches!(error, VimEditError::StartNeovim { .. }));
        assert!(error.to_string().contains("definitely-missing-bcode-nvim"));
    }

    #[test]
    fn search_edit_works_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "alpha beta gamma").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![
                VimEditStep::Keys {
                    input: "/beta<CR>".to_string(),
                },
                VimEditStep::Keys {
                    input: "ciw".to_string(),
                },
                VimEditStep::Insert {
                    text: "delta".to_string(),
                },
                VimEditStep::Keys {
                    input: "<Esc>".to_string(),
                },
            ],
            mode: VimEditMode::Apply,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("run vim edit");

        assert!(result.changed);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read edited"),
            "alpha delta gamma"
        );
    }

    #[test]
    fn substitution_ex_command_works_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo foo\nfoo").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![VimEditStep::Ex {
                command: "%s/foo/bar/g".to_string(),
            }],
            mode: VimEditMode::Apply,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("run vim edit");

        assert!(result.changed);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read edited"),
            "bar bar\nbar"
        );
    }

    #[test]
    fn failed_apply_does_not_modify_original_file() {
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let error = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![VimEditStep::Ex {
                command: "write /tmp/bcode-vim-edit-should-not-exist".to_string(),
            }],
            mode: VimEditMode::Apply,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect_err("unsafe apply should fail");
        assert!(matches!(
            error,
            VimEditError::StepFailed {
                message,
                ..
            } if message.contains("rejected by sandbox")
        ));
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo"
        );
    }

    #[test]
    fn timeout_returns_error_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo").expect("write original");
        let error = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: Vec::new(),
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_nanos(1),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect_err("timeout should error");
        assert!(matches!(error, VimEditError::Timeout { .. }));
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo"
        );
    }

    #[test]
    #[cfg(unix)]
    fn forced_termination_reaps_running_child() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut child = Command::new("sleep")
                .arg("30")
                .kill_on_drop(true)
                .spawn()
                .expect("spawn fixture");
            let fixture_pid = child.id().expect("fixture pid");

            let termination = terminate_child(&mut child, false)
                .await
                .expect("terminate fixture");
            assert_eq!(termination.pid, Some(fixture_pid));
            assert!(!termination.graceful);
            assert!(termination.forced);
            assert!(!termination.exit_status.success());
            assert!(child.try_wait().expect("inspect reaped child").is_some());
        });
    }

    #[test]
    fn operation_timeout_reaps_rpc_ready_neovim() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let session = NeovimSession::start(Path::new(NVIM_EXECUTABLE))
                .await
                .expect("start Neovim");
            session
                .configure_isolation()
                .await
                .expect("configure Neovim");
            let pid = session.child.id();
            let timed_out =
                time::timeout(Duration::from_millis(10), session.nvim.command("sleep 10")).await;
            assert!(timed_out.is_err(), "RPC operation should time out");

            let termination = session.shutdown().await.expect("shutdown Neovim");
            assert_eq!(termination.pid, pid);
            assert!(termination.exit_status.success() || termination.forced);
        });
    }

    #[test]
    fn preview_does_not_modify_original_file_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo bar baz").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![
                VimEditStep::Keys {
                    input: "w".to_string(),
                },
                VimEditStep::Keys {
                    input: "ciw".to_string(),
                },
                VimEditStep::Insert {
                    text: "qux".to_string(),
                },
                VimEditStep::Keys {
                    input: "<Esc>".to_string(),
                },
            ],
            mode: VimEditMode::Preview,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("run vim edit");

        assert!(result.changed);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo bar baz"
        );
    }

    #[test]
    fn apply_modifies_original_file_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo bar baz").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![
                VimEditStep::Keys {
                    input: "w".to_string(),
                },
                VimEditStep::Keys {
                    input: "ciw".to_string(),
                },
                VimEditStep::Insert {
                    text: "qux".to_string(),
                },
                VimEditStep::Keys {
                    input: "<Esc>".to_string(),
                },
            ],
            mode: VimEditMode::Apply,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("run vim edit");

        assert!(result.changed);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo qux baz"
        );
    }

    #[test]
    fn apply_preserves_final_newline_when_nvim_is_available() {
        if !nvim_available() {
            eprintln!("skipping Neovim integration test because `nvim` is not available");
            return;
        }
        let file = NamedTempFile::new().expect("temp file");
        fs::write(file.path(), "foo bar baz\n").expect("write original");
        let result = run_vim_edit(VimEditRequest {
            path: file.path().to_path_buf(),
            nvim_executable: None,
            steps: vec![
                VimEditStep::Keys {
                    input: "w".to_string(),
                },
                VimEditStep::Keys {
                    input: "ciw".to_string(),
                },
                VimEditStep::Insert {
                    text: "qux".to_string(),
                },
                VimEditStep::Keys {
                    input: "<Esc>".to_string(),
                },
            ],
            mode: VimEditMode::Apply,
            sandbox: VimEditSandbox::Default,
            timeout: Duration::from_secs(5),
            observation_granularity: VimEditObservationGranularity::Step,
        })
        .expect("run vim edit");

        assert!(result.changed);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo qux baz\n"
        );
    }

    fn nvim_available() -> bool {
        std::process::Command::new(NVIM_EXECUTABLE)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
