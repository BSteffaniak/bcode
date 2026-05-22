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
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};

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
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runtime status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRuntimeStatus {
    pub running: usize,
    pub queued: usize,
    pub completed: u64,
    pub failed: u64,
}

#[derive(Debug)]
struct RuntimeMetrics {
    running: usize,
    queued: usize,
    completed: u64,
    failed: u64,
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
    running: Arc<Mutex<BTreeMap<ToolExecutionId, Instant>>>,
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
        }
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
        if request.program.trim().is_empty() {
            return Err(ToolRuntimeError::InvalidRequest(
                "program must not be empty".to_string(),
            ));
        }
        let id = ToolExecutionId(self.next_id.fetch_add(1, Ordering::Relaxed));
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
        self.running.lock().await.insert(id, Instant::now());
        let result = run_process_inner(id, request).await;
        drop(permit);
        self.running.lock().await.remove(&id);
        {
            let mut metrics = self.metrics.lock().await;
            metrics.running = metrics.running.saturating_sub(1);
            if result.is_ok() {
                metrics.completed += 1;
            } else {
                metrics.failed += 1;
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
    let mut child = command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(read_limited(stdout, request.max_output_bytes));
    let stderr_task = tokio::spawn(read_limited(stderr, request.max_output_bytes));
    let (status, timed_out) = if let Some(timeout) = request.timeout {
        if let Ok(status) = tokio::time::timeout(timeout, child.wait()).await {
            (status?, false)
        } else {
            let _ = child.kill().await;
            (child.wait().await?, true)
        }
    } else {
        (child.wait().await?, false)
    };
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    Ok(ProcessExecutionResult {
        id,
        exit_code: status.code(),
        timed_out,
        stdout,
        stderr,
    })
}

async fn read_limited(
    reader: Option<impl tokio::io::AsyncRead + Unpin>,
    max_bytes: usize,
) -> Result<Vec<u8>, std::io::Error> {
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        if remaining == 0 {
            continue;
        }
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(output)
}
