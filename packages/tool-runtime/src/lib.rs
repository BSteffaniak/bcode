#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Managed tool execution runtime primitives for Bcode.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, Semaphore};

/// Unique managed tool execution identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolExecutionId(u64);

impl ToolExecutionId {
    /// Return the numeric execution identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Managed process execution request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExecutionRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub timeout: Option<Duration>,
    pub max_output_bytes: usize,
}

/// Managed process execution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExecutionResult {
    pub id: ToolExecutionId,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Incremental process output event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutputEvent {
    pub id: ToolExecutionId,
    pub stream: ProcessOutputStream,
    pub sequence: u64,
    pub bytes: Vec<u8>,
}

/// Process output stream identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutputStream {
    Stdout,
    Stderr,
}

/// Runtime status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRuntimeStatus {
    pub running: usize,
    pub queued: usize,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
}

#[derive(Debug)]
struct RuntimeMetrics {
    running: usize,
    queued: usize,
    completed: u64,
    failed: u64,
    cancelled: u64,
}

#[derive(Debug)]
struct RunningExecution {
    cancel: Arc<Notify>,
}

/// Cancellation handle for a managed tool execution.
#[derive(Debug, Clone)]
pub struct ToolExecutionCancelHandle {
    id: ToolExecutionId,
    notify: Arc<Notify>,
}

impl ToolExecutionCancelHandle {
    /// Return the managed execution identifier.
    #[must_use]
    pub const fn id(&self) -> ToolExecutionId {
        self.id
    }

    /// Request cancellation of the managed execution.
    pub fn cancel(&self) {
        self.notify.notify_waiters();
    }
}

/// Errors from managed tool execution.
#[derive(Debug, Error)]
pub enum ToolRuntimeError {
    #[error("invalid process request: {0}")]
    InvalidRequest(String),
    #[error("process IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("process task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("process execution was cancelled")]
    Cancelled,
}

/// Managed runtime for process-backed tool executions.
#[derive(Debug, Clone)]
pub struct ToolExecutionRuntime {
    next_id: Arc<AtomicU64>,
    semaphore: Arc<Semaphore>,
    metrics: Arc<Mutex<RuntimeMetrics>>,
    running: Arc<Mutex<BTreeMap<ToolExecutionId, RunningExecution>>>,
}

impl ToolExecutionRuntime {
    /// Create a runtime with a maximum number of concurrently running executions.
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.max(1);
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            metrics: Arc::new(Mutex::new(RuntimeMetrics {
                running: 0,
                queued: 0,
                completed: 0,
                failed: 0,
                cancelled: 0,
            })),
            running: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Return a runtime status snapshot.
    pub async fn status(&self) -> ToolRuntimeStatus {
        let metrics = self.metrics.lock().await;
        ToolRuntimeStatus {
            running: metrics.running,
            queued: metrics.queued,
            completed: metrics.completed,
            failed: metrics.failed,
            cancelled: metrics.cancelled,
        }
    }

    /// Create a cancellation handle for the next managed process execution.
    #[must_use]
    pub fn cancellation_handle(&self) -> ToolExecutionCancelHandle {
        ToolExecutionCancelHandle {
            id: ToolExecutionId(self.next_id.load(Ordering::Relaxed)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Cancel a currently running execution by ID.
    pub async fn cancel(&self, id: ToolExecutionId) -> bool {
        let cancel = {
            let running = self.running.lock().await;
            running
                .get(&id)
                .map(|execution| Arc::clone(&execution.cancel))
        };
        if let Some(cancel) = cancel {
            cancel.notify_waiters();
            return true;
        }
        false
    }

    /// Run a process to completion under runtime accounting and concurrency limits.
    ///
    /// # Errors
    ///
    /// Returns an error when process spawning, output collection, or task joining fails.
    pub async fn run_process(
        &self,
        request: ProcessExecutionRequest,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        self.run_process_streaming(request, |_| {}).await
    }

    /// Run a process and emit output events as chunks are read.
    ///
    /// # Errors
    ///
    /// Returns an error when process spawning, output collection, or task joining fails.
    pub async fn run_process_streaming(
        &self,
        request: ProcessExecutionRequest,
        on_output: impl FnMut(ProcessOutputEvent) + Send + 'static,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        if request.program.trim().is_empty() {
            return Err(ToolRuntimeError::InvalidRequest(
                "program must not be empty".to_string(),
            ));
        }
        let id = ToolExecutionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let cancel = Arc::new(Notify::new());
        {
            let mut metrics = self.metrics.lock().await;
            metrics.queued += 1;
        }
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ToolRuntimeError::Cancelled)?;
        {
            let mut metrics = self.metrics.lock().await;
            metrics.queued = metrics.queued.saturating_sub(1);
            metrics.running += 1;
        }
        self.running.lock().await.insert(
            id,
            RunningExecution {
                cancel: Arc::clone(&cancel),
            },
        );
        let result = run_process_inner(id, request, cancel, on_output).await;
        drop(permit);
        self.running.lock().await.remove(&id);
        {
            let mut metrics = self.metrics.lock().await;
            metrics.running = metrics.running.saturating_sub(1);
            match &result {
                Ok(result) if result.cancelled => metrics.cancelled += 1,
                Ok(_) => metrics.completed += 1,
                Err(_) => metrics.failed += 1,
            }
        }
        result
    }
}

impl Default for ToolExecutionRuntime {
    fn default() -> Self {
        Self::new(4)
    }
}

async fn run_process_inner(
    id: ToolExecutionId,
    request: ProcessExecutionRequest,
    cancel: Arc<Notify>,
    on_output: impl FnMut(ProcessOutputEvent) + Send + 'static,
) -> Result<ProcessExecutionResult, ToolRuntimeError> {
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = request.cwd {
        command.current_dir(cwd);
    }
    configure_command_for_timeout(&mut command);
    let mut child = command.kill_on_drop(true).spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let output_callback = Arc::new(std::sync::Mutex::new(on_output));
    let stdout_task = tokio::spawn(read_limited(
        id,
        ProcessOutputStream::Stdout,
        stdout,
        request.max_output_bytes,
        Arc::clone(&output_callback),
    ));
    let stderr_task = tokio::spawn(read_limited(
        id,
        ProcessOutputStream::Stderr,
        stderr,
        request.max_output_bytes,
        output_callback,
    ));
    let (status, timed_out, cancelled) =
        wait_for_process(&mut child, request.timeout, cancel).await?;
    let stdout = if timed_out {
        Vec::new()
    } else {
        stdout_task.await??
    };
    let stderr = if timed_out {
        Vec::new()
    } else {
        stderr_task.await??
    };
    Ok(ProcessExecutionResult {
        id,
        exit_code: status.code(),
        timed_out,
        cancelled,
        stdout,
        stderr,
    })
}

async fn wait_for_process(
    child: &mut Child,
    timeout: Option<Duration>,
    cancel: Arc<Notify>,
) -> Result<(std::process::ExitStatus, bool, bool), std::io::Error> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false, false));
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            return terminate_child_after_timeout(child)
                .await
                .map(|status| (status, true, false));
        }
        if tokio::time::timeout(Duration::from_millis(10), cancel.notified())
            .await
            .is_ok()
        {
            return terminate_child_after_timeout(child)
                .await
                .map(|status| (status, false, true));
        }
    }
}

#[cfg(unix)]
fn configure_command_for_timeout(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_command_for_timeout(_command: &mut Command) {}

#[cfg(unix)]
async fn terminate_child_after_timeout(
    child: &mut Child,
) -> Result<std::process::ExitStatus, std::io::Error> {
    let Some(child_id) = child.id() else {
        return child.wait().await;
    };
    let process_group_id = i32::try_from(child_id).unwrap_or(i32::MAX);
    let _ = send_signal_to_process_group(process_group_id, SIGTERM);
    let grace_deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= grace_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = send_signal_to_process_group(process_group_id, SIGKILL);
    child.wait().await
}

#[cfg(not(unix))]
async fn terminate_child_after_timeout(
    child: &mut Child,
) -> Result<std::process::ExitStatus, std::io::Error> {
    let _ = child.kill().await;
    child.wait().await
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;
#[cfg(unix)]
const ESRCH: i32 = 3;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
fn send_signal_to_process_group(process_group_id: i32, signal: i32) -> Result<(), std::io::Error> {
    let target = -process_group_id;
    // SAFETY: `kill` is called with a process-group target created by `process_group(0)`.
    let result = unsafe { kill(target, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        return Ok(());
    }
    Err(error)
}

async fn read_limited(
    id: ToolExecutionId,
    stream: ProcessOutputStream,
    reader: Option<impl tokio::io::AsyncRead + Unpin>,
    max_bytes: usize,
    on_output: Arc<std::sync::Mutex<impl FnMut(ProcessOutputEvent)>>,
) -> Result<Vec<u8>, std::io::Error> {
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut sequence = 0_u64;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        sequence = sequence.saturating_add(1);
        if let Ok(mut on_output) = on_output.lock() {
            on_output(ProcessOutputEvent {
                id,
                stream,
                sequence,
                bytes: buffer[..read].to_vec(),
            });
        }
        let remaining = max_bytes.saturating_sub(output.len());
        if remaining == 0 {
            continue;
        }
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_timeout_is_recorded() {
        let runtime = ToolExecutionRuntime::new(1);
        let result = runtime
            .run_process(ProcessExecutionRequest {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 5".to_string()],
                cwd: None,
                timeout: Some(Duration::from_millis(100)),
                max_output_bytes: 1024,
            })
            .await
            .expect("process returns timeout result");
        assert!(result.timed_out);
        assert!(!result.cancelled);
        let status = runtime.status().await;
        assert_eq!(status.running, 0);
        assert_eq!(status.completed, 1);
    }

    #[tokio::test]
    async fn process_output_is_limited() {
        let runtime = ToolExecutionRuntime::new(1);
        let result = runtime
            .run_process(ProcessExecutionRequest {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "printf abcdef".to_string()],
                cwd: None,
                timeout: Some(Duration::from_secs(1)),
                max_output_bytes: 3,
            })
            .await
            .expect("process returns output");
        assert_eq!(result.stdout, b"abc");
        assert_eq!(result.stderr, b"");
    }
}
