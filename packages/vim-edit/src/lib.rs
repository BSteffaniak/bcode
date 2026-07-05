#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Neovim RPC backed Vim editing for Bcode.
//!
//! This crate owns reusable Vim edit behavior. It starts isolated headless
//! Neovim processes, drives them through RPC, records state after each edit
//! step, and optionally writes the final buffer back to the requested file.

use bcode_tui_components::diff_viewer::{DiffLineKind, diff_from_text};
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio as nvim_create;
use nvim_rs::rpc::handler::Dummy;
use nvim_rs::{Neovim, Value};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
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

/// Error returned while running a Vim edit operation.
#[derive(Debug, Error)]
pub enum VimEditError {
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
    /// A Vim edit step was rejected by sandbox policy.
    #[error("step {step_index} rejected by sandbox: {reason}")]
    UnsafeCommand {
        /// Rejected step index.
        step_index: usize,
        /// Rejection reason.
        reason: String,
    },
    /// The operation timed out.
    #[error("Vim edit operation timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u128 },
    /// Failed to write the final buffer back to the requested path.
    #[error("failed to write {path:?}: {source}")]
    WriteFile { path: PathBuf, source: io::Error },
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
    let timeout = request.timeout;
    Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| VimEditError::StartNeovim {
            executable: "tokio runtime".to_string(),
            source,
        })?
        .block_on(async move {
            time::timeout(timeout, run_vim_edit_inner(request))
                .await
                .unwrap_or(Err(VimEditError::Timeout {
                    timeout_ms: timeout.as_millis(),
                }))
        })
}

async fn run_vim_edit_inner(request: VimEditRequest) -> Result<VimEditResult, VimEditError> {
    let original = read_text_file(&request.path)?;
    let temp_file = temp_file_with_contents(&original)?;
    let session = NeovimSession::start(
        temp_file.path(),
        request
            .nvim_executable
            .as_deref()
            .unwrap_or_else(|| Path::new(NVIM_EXECUTABLE)),
    )
    .await?;
    session.configure_isolation().await?;

    let mut previous_buffer = session.buffer_text().await?;
    let mut events = Vec::with_capacity(request.steps.len());
    for (step_index, step) in request.steps.iter().enumerate() {
        reject_unsafe_step(step, step_index, request.sandbox)?;
        let before_cursor = session.cursor().await?;
        session.apply_step(step).await?;
        let after_cursor = session.cursor().await?;
        let nvim_mode = session.mode().await?;
        let context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
        let next_buffer = session.buffer_text().await?;
        let changed = next_buffer != previous_buffer;
        previous_buffer = next_buffer;
        events.push(VimEditEvent {
            step_index,
            step: step.clone(),
            before_cursor,
            after_cursor,
            nvim_mode,
            context,
            changed,
            message: None,
        });
    }

    let final_text = previous_buffer;
    let cursor = session.cursor().await?;
    let nvim_mode = session.mode().await?;
    let final_context = session.context(DEFAULT_CONTEXT_RADIUS).await?;
    session.shutdown().await;

    let changed = original != final_text;
    if request.mode == VimEditMode::Apply && changed {
        fs::write(&request.path, final_text.as_bytes()).map_err(|source| {
            VimEditError::WriteFile {
                path: request.path.clone(),
                source,
            }
        })?;
    }

    Ok(VimEditResult {
        changed,
        diff: render_diff(&request.path, &original, &final_text),
        cursor,
        nvim_mode,
        final_context,
        events,
    })
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

struct NeovimSession {
    nvim: Neovim<Compat<ChildStdin>>,
    io_handle: JoinHandle<Result<(), Box<nvim_rs::error::LoopError>>>,
    child: Child,
}

impl NeovimSession {
    async fn start(path: &Path, executable: &Path) -> Result<Self, VimEditError> {
        let mut command = Command::new(executable);
        command
            .arg("--headless")
            .arg("--clean")
            .arg("-n")
            .arg("--noplugin")
            .arg("--embed")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (nvim, io_handle, child) =
            nvim_create::new_child_cmd(&mut command, Dummy::<Compat<ChildStdin>>::new())
                .await
                .map_err(|source| VimEditError::StartNeovim {
                    executable: executable.display().to_string(),
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
            "set nomodeline",
            "set noexrc",
            "set noswapfile",
            "set encoding=utf-8",
            "set fileencoding=utf-8",
        ] {
            self.nvim_command(command).await?;
        }
        Ok(())
    }

    async fn apply_step(&self, step: &VimEditStep) -> Result<(), VimEditError> {
        match step {
            VimEditStep::Keys { input } => {
                let keys = self
                    .nvim
                    .replace_termcodes(input, true, true, true)
                    .await
                    .map_err(|error| VimEditError::Rpc(error.to_string()))?;
                self.nvim
                    .feedkeys(&keys, "x", false)
                    .await
                    .map_err(|error| VimEditError::Rpc(error.to_string()))
            }
            VimEditStep::Insert { text } => self
                .nvim
                .paste(text, false, -1)
                .await
                .map(|_| ())
                .map_err(|error| VimEditError::Rpc(error.to_string())),
            VimEditStep::Ex { command } => self.nvim_command(command).await,
        }
    }

    async fn nvim_command(&self, command: &str) -> Result<(), VimEditError> {
        self.nvim
            .command(command)
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))
    }

    async fn buffer_text(&self) -> Result<String, VimEditError> {
        let buffer = self
            .nvim
            .get_current_buf()
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))?;
        let lines = buffer
            .get_lines(0, -1, false)
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))?;
        Ok(lines.join("\n"))
    }

    async fn cursor(&self) -> Result<CursorPosition, VimEditError> {
        let window = self
            .nvim
            .get_current_win()
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))?;
        let (line, column) = window
            .get_cursor()
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))?;
        Ok(CursorPosition {
            line: nonnegative_i64_to_usize(line).saturating_add(0),
            column: nonnegative_i64_to_usize(column).saturating_add(1),
        })
    }

    async fn mode(&self) -> Result<String, VimEditError> {
        let values = self
            .nvim
            .get_mode()
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))?;
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
        let buffer = self
            .nvim
            .get_current_buf()
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))?;
        let start_line = cursor.line.saturating_sub(radius).max(1);
        let end_line = cursor.line.saturating_add(radius);
        let lines = buffer
            .get_lines(
                usize_to_i64(start_line.saturating_sub(1)),
                usize_to_i64(end_line),
                false,
            )
            .await
            .map_err(|error| VimEditError::Rpc(error.to_string()))?;
        Ok(TextContext { start_line, lines })
    }

    async fn shutdown(mut self) {
        let _ = self.nvim.command("qa!").await;
        let _ = time::timeout(Duration::from_millis(500), &mut self.io_handle).await;
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.child.kill().await;
            }
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
    let trimmed = command.trim_start();
    let trimmed = trimmed.strip_prefix(':').unwrap_or(trimmed).trim_start();
    let command_name = trimmed
        .split(|character: char| character.is_ascii_whitespace() || character == '!')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if trimmed.starts_with('!') || UNSAFE_EX_COMMANDS.contains(&command_name.as_str()) {
        return Err(VimEditError::UnsafeCommand {
            step_index,
            reason: format!("Ex command `{command_name}` is blocked in default sandbox mode"),
        });
    }
    Ok(())
}

fn reject_unsafe_keys(input: &str, step_index: usize) -> Result<(), VimEditError> {
    let lower = input.to_ascii_lowercase();
    for marker in [
        ":!", ":write", ":w", ":edit", ":e", ":read", ":r", ":source", ":so",
    ] {
        if lower.contains(marker) {
            return Err(VimEditError::UnsafeCommand {
                step_index,
                reason: format!("key input contains blocked command-line marker `{marker}`"),
            });
        }
    }
    Ok(())
}

const UNSAFE_EX_COMMANDS: &[&str] = &[
    "!", "read", "r", "write", "w", "edit", "e", "source", "so", "lua", "python", "python3",
    "perl", "ruby", "terminal", "term", "make", "grep", "vimgrep", "cexpr", "cgetexpr",
];

fn render_diff(path: &Path, old_text: &str, new_text: &str) -> String {
    let document = diff_from_text(&path.display().to_string(), old_text, new_text);
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
        })
        .expect_err("missing nvim should error");
        assert!(matches!(error, VimEditError::StartNeovim { .. }));
        assert!(error.to_string().contains("definitely-missing-bcode-nvim"));
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
        })
        .expect("run vim edit");

        assert!(result.changed);
        assert_eq!(
            fs::read_to_string(file.path()).expect("read original"),
            "foo qux baz"
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
