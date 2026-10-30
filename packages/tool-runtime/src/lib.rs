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
use tokio::sync::{Notify, Semaphore};

struct ProcessObservation {
    capture: Option<bcode_xray::CaptureContext>,
    id: ToolExecutionId,
    started: Instant,
    finished: bool,
}

impl ProcessObservation {
    fn new(id: ToolExecutionId) -> Self {
        let observation = Self {
            capture: bcode_xray::CaptureContext::current(),
            id,
            started: Instant::now(),
            finished: false,
        };
        observation.emit("queued", &serde_json::Value::Null);
        observation
    }

    fn emit(&self, name: &str, data: &serde_json::Value) {
        if let Some(capture) = &self.capture {
            capture.emit("tool_process", name, serde_json::json!({
                "execution_id": self.id.get(),
                "elapsed_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "details": data,
            }), false);
        }
    }

    fn finish(&mut self, outcome: &str) {
        self.finished = true;
        self.emit("finished", &serde_json::json!({"outcome": outcome}));
    }
}

impl Drop for ProcessObservation {
    fn drop(&mut self) {
        if !self.finished {
            self.emit(
                "unfinished",
                &serde_json::json!({"cleanup_confirmed": false}),
            );
        }
    }
}

struct OutputReaderCleanup {
    stdout: tokio::task::AbortHandle,
    stderr: tokio::task::AbortHandle,
}

impl Drop for OutputReaderCleanup {
    fn drop(&mut self) {
        // JoinHandle drop detaches: cancellation/error must not orphan pipe readers.
        self.stdout.abort();
        self.stderr.abort();
    }
}

async fn process_capture_scope<F: std::future::Future>(
    capture: Option<bcode_xray::CaptureContext>,
    future: F,
) -> F::Output {
    if let Some(capture) = capture {
        capture.scope(future).await
    } else {
        future.await
    }
}

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
    /// Whether to inherit the parent process environment.
    pub inherit_environment: bool,
    /// Explicit environment entries applied after inheritance/clear policy.
    pub environment: BTreeMap<String, String>,
    /// Environment variable names removed after inheritance and explicit entries.
    pub remove_environment: Vec<String>,
}

/// Captured process output stream bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCapturedOutput {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Normalized terminal process outcome owned by the managed process boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessTermination {
    /// The process exited normally with an exact code.
    Exited { code: i32 },
    /// The process was terminated by a platform signal.
    Signaled { signal: i32 },
    /// The platform returned neither a normal exit code nor a supported signal identity.
    Unknown,
}

/// Managed process execution result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExecutionResult {
    pub id: ToolExecutionId,
    pub termination: ProcessTermination,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub stdout: ProcessCapturedOutput,
    pub stderr: ProcessCapturedOutput,
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

#[derive(Debug, Default)]
struct ProcessCancellation {
    requested: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl ProcessCancellation {
    fn cancel(&self) {
        self.requested.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.requested.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug)]
struct RunningExecution {
    cancel: Arc<ProcessCancellation>,
}

/// Synchronous, short accounting critical sections allow caller-abort cleanup in `Drop`.
struct ExecutionAccounting<'a> {
    runtime: &'a ToolExecutionRuntime,
    id: ToolExecutionId,
    running: bool,
    finished: bool,
}

impl ExecutionAccounting<'_> {
    fn start_running(&mut self) {
        let mut metrics = self
            .runtime
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        metrics.queued = metrics.queued.saturating_sub(1);
        metrics.running += 1;
        drop(metrics);
        self.running = true;
    }

    fn finish(&mut self, outcome: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.runtime
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        let mut metrics = self
            .runtime
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.running {
            metrics.running = metrics.running.saturating_sub(1);
        } else {
            metrics.queued = metrics.queued.saturating_sub(1);
        }
        match outcome {
            "completed" | "timed_out" => metrics.completed += 1,
            "cancelled" | "abandoned" => metrics.cancelled += 1,
            _ => metrics.failed += 1,
        }
    }
}

impl Drop for ExecutionAccounting<'_> {
    fn drop(&mut self) {
        // Caller abandonment cancels local work; it does not establish an OS outcome.
        self.finish("abandoned");
    }
}

/// Cancellation handle for a managed tool execution.
#[derive(Debug, Clone)]
pub struct ToolExecutionCancelHandle {
    id: ToolExecutionId,
    notify: Arc<ProcessCancellation>,
}

impl ToolExecutionCancelHandle {
    /// Return the managed execution identifier.
    #[must_use]
    pub const fn id(&self) -> ToolExecutionId {
        self.id
    }

    /// Request cancellation of the managed execution.
    pub fn cancel(&self) {
        self.notify.cancel();
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
    metrics: Arc<std::sync::Mutex<RuntimeMetrics>>,
    running: Arc<std::sync::Mutex<BTreeMap<ToolExecutionId, RunningExecution>>>,
}

impl ToolExecutionRuntime {
    /// Create a runtime with a maximum number of concurrently running executions.
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.max(1);
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            metrics: Arc::new(std::sync::Mutex::new(RuntimeMetrics {
                running: 0,
                queued: 0,
                completed: 0,
                failed: 0,
                cancelled: 0,
            })),
            running: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    /// Return an immediately ready runtime status snapshot.
    #[must_use = "await the snapshot to consume its value"]
    pub fn status(&self) -> impl std::future::Future<Output = ToolRuntimeStatus> {
        let metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = ToolRuntimeStatus {
            running: metrics.running,
            queued: metrics.queued,
            completed: metrics.completed,
            failed: metrics.failed,
            cancelled: metrics.cancelled,
        };
        drop(metrics);
        std::future::ready(status)
    }

    /// Reserve a unique identifier and cancellation handle for a managed execution.
    #[must_use]
    pub fn cancellation_handle(&self) -> ToolExecutionCancelHandle {
        ToolExecutionCancelHandle {
            id: ToolExecutionId(self.next_id.fetch_add(1, Ordering::Relaxed)),
            notify: Arc::new(ProcessCancellation::default()),
        }
    }

    /// Cancel a queued or currently running execution by ID and return a ready result.
    ///
    /// Cancellation is signalled immediately when this method is called.
    #[must_use = "await the result to determine whether the execution was registered"]
    pub fn cancel(&self, id: ToolExecutionId) -> impl std::future::Future<Output = bool> {
        let cancel = {
            let running = self
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            running
                .get(&id)
                .map(|execution| Arc::clone(&execution.cancel))
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
            return std::future::ready(true);
        }
        std::future::ready(false)
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

    /// Run one process with a caller-owned cancellation handle.
    ///
    /// # Errors
    ///
    /// Returns an error when process spawning, output collection, or task joining fails.
    pub async fn run_process_cancellable(
        &self,
        request: ProcessExecutionRequest,
        cancel: &ToolExecutionCancelHandle,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        self.run_process_streaming_with_cancel(request, cancel, |_| {})
            .await
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
        let cancel = self.cancellation_handle();
        self.run_process_streaming_with_cancel(request, &cancel, on_output)
            .await
    }

    async fn run_process_streaming_with_cancel(
        &self,
        request: ProcessExecutionRequest,
        cancel: &ToolExecutionCancelHandle,
        on_output: impl FnMut(ProcessOutputEvent) + Send + 'static,
    ) -> Result<ProcessExecutionResult, ToolRuntimeError> {
        if request.program.trim().is_empty() {
            return Err(ToolRuntimeError::InvalidRequest(
                "program must not be empty".to_string(),
            ));
        }
        let id = cancel.id;
        let mut observation = ProcessObservation::new(id);
        self.next_id
            .fetch_max(id.0.saturating_add(1), Ordering::Relaxed);
        let cancel_notify = Arc::clone(&cancel.notify);
        {
            let mut metrics = self
                .metrics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            metrics.queued += 1;
        }
        let mut accounting = ExecutionAccounting {
            runtime: self,
            id,
            running: false,
            finished: false,
        };
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                RunningExecution {
                    cancel: Arc::clone(&cancel_notify),
                },
            );
        let permit = tokio::select! {
            biased;
            () = cancel_notify.cancelled() => {
                observation.emit("cancellation_observed", &serde_json::json!({"phase": "queued"}));
                accounting.finish("cancelled");
                observation.emit("retired", &serde_json::json!({"spawned": false}));
                observation.finish("cancelled_before_spawn");
                return Err(ToolRuntimeError::Cancelled);
            }
            permit = self.semaphore.clone().acquire_owned() => if let Ok(permit) = permit {
                permit
            } else {
                accounting.finish("cancelled");
                observation.emit("retired", &serde_json::json!({"spawned": false}));
                observation.finish("admission_closed");
                return Err(ToolRuntimeError::Cancelled);
            },
        };
        accounting.start_running();
        observation.emit("started", &serde_json::Value::Null);
        let result = run_process_inner(id, request, cancel_notify, on_output).await;
        drop(permit);
        let outcome = match &result {
            Ok(result) if result.cancelled => "cancelled",
            Ok(result) if result.timed_out => "timed_out",
            Ok(_) => "completed",
            Err(ToolRuntimeError::Cancelled) => "cancelled",
            Err(_) => "failed",
        };
        accounting.finish(outcome);
        observation.emit(
            "retired",
            &serde_json::json!({"running_registration_removed": true}),
        );
        observation.finish(outcome);
        result
    }
}

impl Default for ToolExecutionRuntime {
    fn default() -> Self {
        Self::new(4)
    }
}

fn process_command(request: &ProcessExecutionRequest) -> Command {
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !request.inherit_environment {
        command.env_clear();
    }
    command.envs(&request.environment);
    for name in &request.remove_environment {
        command.env_remove(name);
    }
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    configure_command_for_timeout(&mut command);
    command
}

async fn run_process_inner(
    id: ToolExecutionId,
    request: ProcessExecutionRequest,
    cancel: Arc<ProcessCancellation>,
    on_output: impl FnMut(ProcessOutputEvent) + Send + 'static,
) -> Result<ProcessExecutionResult, ToolRuntimeError> {
    if cancel.requested.load(Ordering::Acquire) {
        return Err(ToolRuntimeError::Cancelled);
    }
    let mut command = process_command(&request);
    let started = Instant::now();
    let mut child = command.kill_on_drop(true).spawn()?;
    #[cfg(windows)]
    let process_guard = attach_child_process_guard(&child)?;
    #[cfg(not(windows))]
    let process_guard = attach_child_process_guard(&child);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let output_callback = Arc::new(std::sync::Mutex::new(on_output));
    let capture = bcode_xray::CaptureContext::current();
    if let Some(capture) = &capture {
        capture.emit(
            "tool_process",
            "spawned",
            serde_json::json!({"execution_id": id.get()}),
            false,
        );
    }
    let stdout_task = tokio::spawn(process_capture_scope(
        capture.clone(),
        read_limited(
            id,
            ProcessOutputStream::Stdout,
            stdout,
            request.max_output_bytes,
            Arc::clone(&output_callback),
        ),
    ));
    let stderr_task = tokio::spawn(process_capture_scope(
        capture.clone(),
        read_limited(
            id,
            ProcessOutputStream::Stderr,
            stderr,
            request.max_output_bytes,
            output_callback,
        ),
    ));
    let _reader_cleanup = OutputReaderCleanup {
        stdout: stdout_task.abort_handle(),
        stderr: stderr_task.abort_handle(),
    };
    let (status, timed_out, cancelled) =
        wait_for_process(id, &mut child, request.timeout, cancel, &process_guard).await?;
    if let Some(capture) = &capture {
        capture.emit("tool_process", "reaped", serde_json::json!({
            "execution_id": id.get(), "timed_out": timed_out, "cancelled": cancelled,
            "termination": match normalize_process_termination(status) {
                ProcessTermination::Exited { code } => serde_json::json!({"exited": code}),
                ProcessTermination::Signaled { signal } => serde_json::json!({"signal": signal}),
                ProcessTermination::Unknown => serde_json::Value::Null,
            },
        }), false);
    }
    let (stdout, stdout_truncated) = collect_process_output(stdout_task, timed_out).await?;
    let (stderr, stderr_truncated) = collect_process_output(stderr_task, timed_out).await?;
    if let Some(capture) = &capture {
        capture.emit(
            "tool_process",
            "output_collection_finished",
            serde_json::json!({
                "execution_id": id.get(), "omitted_after_timeout": timed_out,
                "stdout_bytes": stdout.len(), "stderr_bytes": stderr.len(),
                "stdout_truncated": stdout_truncated, "stderr_truncated": stderr_truncated,
            }),
            false,
        );
    }
    Ok(ProcessExecutionResult {
        id,
        termination: normalize_process_termination(status),
        timed_out,
        cancelled,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        stdout: ProcessCapturedOutput {
            bytes: stdout,
            truncated: stdout_truncated,
        },
        stderr: ProcessCapturedOutput {
            bytes: stderr,
            truncated: stderr_truncated,
        },
    })
}

async fn collect_process_output(
    task: tokio::task::JoinHandle<Result<(Vec<u8>, bool), std::io::Error>>,
    timed_out: bool,
) -> Result<(Vec<u8>, bool), ToolRuntimeError> {
    if timed_out {
        task.abort();
        let _ = task.await;
        Ok((Vec::new(), false))
    } else {
        Ok(task.await??)
    }
}

fn normalize_process_termination(status: std::process::ExitStatus) -> ProcessTermination {
    if let Some(code) = status.code() {
        return ProcessTermination::Exited { code };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return ProcessTermination::Signaled { signal };
        }
    }
    ProcessTermination::Unknown
}

async fn wait_for_process(
    id: ToolExecutionId,
    child: &mut Child,
    timeout: Option<Duration>,
    cancel: Arc<ProcessCancellation>,
    process_guard: &ChildProcessGuard,
) -> Result<(std::process::ExitStatus, bool, bool), std::io::Error> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false, false));
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            return terminate_child_after_timeout(child, process_guard)
                .await
                .map(|status| (status, true, false));
        }
        if tokio::time::timeout(Duration::from_millis(10), cancel.cancelled())
            .await
            .is_ok()
        {
            if let Some(capture) = bcode_xray::CaptureContext::current() {
                capture.emit(
                    "tool_process",
                    "cancellation_observed",
                    serde_json::json!({"execution_id": id.get(), "phase": "running"}),
                    false,
                );
            }
            return terminate_child_after_timeout(child, process_guard)
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
const fn configure_command_for_timeout(_command: &mut Command) {}

#[cfg(windows)]
const _: fn(&mut Command) = configure_command_for_timeout;

#[cfg(windows)]
struct ChildProcessGuard(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
fn attach_child_process_guard(child: &Child) -> Result<ChildProcessGuard, std::io::Error> {
    use std::mem::size_of;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let process = child.raw_handle().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "spawned tool process handle is unavailable",
        )
    })?;
    // SAFETY: the job handle is checked before use, the limit structure is
    // initialized for the duration of the call, and the process handle is
    // borrowed from a live Tokio child.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let guard = ChildProcessGuard(
            <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(job),
        );
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("job limit structure size fits u32"),
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if AssignProcessToJobObject(job, process.cast()) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(guard)
    }
}

#[cfg(not(windows))]
struct ChildProcessGuard;

#[cfg(not(windows))]
const fn attach_child_process_guard(_child: &Child) -> ChildProcessGuard {
    ChildProcessGuard
}

#[cfg(unix)]
async fn terminate_child_after_timeout(
    child: &mut Child,
    _process_guard: &ChildProcessGuard,
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

#[cfg(windows)]
async fn terminate_child_after_timeout(
    child: &mut Child,
    process_guard: &ChildProcessGuard,
) -> Result<std::process::ExitStatus, std::io::Error> {
    // SAFETY: the guard owns a live job handle configured to contain the
    // spawned process and all descendants. Terminating the job is the Windows
    // equivalent of killing the Unix process group.
    let terminated = unsafe {
        windows_sys::Win32::System::JobObjects::TerminateJobObject(
            std::os::windows::io::AsRawHandle::as_raw_handle(&process_guard.0),
            1,
        )
    };
    if terminated == 0 {
        let error = std::io::Error::last_os_error();
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(error);
    }
    child.wait().await
}

#[cfg(all(not(unix), not(windows)))]
async fn terminate_child_after_timeout(
    child: &mut Child,
    _process_guard: &ChildProcessGuard,
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
) -> Result<(Vec<u8>, bool), std::io::Error> {
    let Some(mut reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 4096];
    let mut sequence = 0_u64;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        sequence = sequence.saturating_add(1);
        if let Some(capture) = bcode_xray::CaptureContext::current() {
            // Raw process output can contain credentials, so retain only geometry here.
            capture.emit(
                "tool_process",
                "output_chunk",
                serde_json::json!({
                    "execution_id": id.get(), "stream": match stream {
                        ProcessOutputStream::Stdout => "stdout",
                        ProcessOutputStream::Stderr => "stderr",
                    },
                    "sequence": sequence, "byte_len": read,
                }),
                false,
            );
        }
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
            truncated = true;
            continue;
        }
        if read > remaining {
            truncated = true;
        }
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok((output, truncated))
}

#[cfg(test)]
mod xray_tests {
    use super::*;

    #[tokio::test]
    async fn queued_cancellation_is_latched_and_retires_accounting_without_spawn() {
        let runtime = ToolExecutionRuntime::new(1);
        let permit = runtime
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("hold admission");
        let cancel = runtime.cancellation_handle();
        let other = runtime.cancellation_handle();
        assert_ne!(cancel.id(), other.id());
        let request = ProcessExecutionRequest {
            program: "must-not-be-spawned".into(),
            args: Vec::new(),
            cwd: None,
            timeout: None,
            max_output_bytes: 16,
            inherit_environment: false,
            environment: BTreeMap::new(),
            remove_environment: Vec::new(),
        };
        let work = runtime.run_process_streaming_with_cancel(request, &cancel, |_| {});
        tokio::pin!(work);
        tokio::select! {
            biased;
            result = &mut work => panic!("admission unexpectedly completed: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        assert_eq!(runtime.status().await.queued, 1);
        assert!(runtime.cancel(cancel.id()).await);
        assert!(matches!(work.await, Err(ToolRuntimeError::Cancelled)));
        assert_eq!(runtime.status().await.queued, 0);
        assert_eq!(runtime.status().await.running, 0);
        assert_eq!(runtime.status().await.cancelled, 1);
        assert!(!runtime.cancel(cancel.id()).await);
        drop(permit);
    }

    #[tokio::test]
    async fn abandoning_a_queued_future_releases_registration_and_queue_count() {
        let runtime = ToolExecutionRuntime::new(1);
        let _permit = runtime
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("hold admission");
        let cancel = runtime.cancellation_handle();
        {
            let request = ProcessExecutionRequest {
                program: "must-not-be-spawned".into(),
                args: Vec::new(),
                cwd: None,
                timeout: None,
                max_output_bytes: 16,
                inherit_environment: false,
                environment: BTreeMap::new(),
                remove_environment: Vec::new(),
            };
            let work = runtime.run_process_streaming_with_cancel(request, &cancel, |_| {});
            tokio::pin!(work);
            tokio::select! {
                biased;
                result = &mut work => panic!("admission unexpectedly completed: {result:?}"),
                () = tokio::task::yield_now() => {}
            }
            assert_eq!(runtime.status().await.queued, 1);
        }
        assert_eq!(runtime.status().await.queued, 0);
        assert_eq!(runtime.status().await.cancelled, 1);
        assert!(!runtime.cancel(cancel.id()).await);
    }

    #[tokio::test]
    async fn cancellation_before_waiter_registration_is_not_lost() {
        let cancellation = ProcessCancellation::default();
        cancellation.cancel();
        cancellation.cancelled().await;
        cancellation.cancelled().await;
    }

    #[tokio::test]
    async fn pipe_reader_observation_uses_explicit_origin_and_never_emits_output_text() {
        let hub = bcode_xray::XrayHub::default();
        let mut observation = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: bcode_xray::XRAY_SCHEMA_VERSION,
                session_id: "session".into(),
                deep_capture: true,
            })
            .expect("observe");
        let capture = hub.context(bcode_xray::XrayCorrelation {
            session_id: "session".into(),
            turn_id: Some("turn".into()),
            tool_call_id: Some("call".into()),
            ..Default::default()
        });
        let bytes = b"private-tool-output";
        let (output, truncated) = tokio::spawn(process_capture_scope(Some(capture), async move {
            read_limited(
                ToolExecutionId(7),
                ProcessOutputStream::Stdout,
                Some(&bytes[..]),
                4,
                Arc::new(std::sync::Mutex::new(|_| {})),
            )
            .await
        }))
        .await
        .expect("join")
        .expect("read");
        assert_eq!(output, b"priv");
        assert!(truncated);
        observation.finalize().expect("cutoff");
        let mut chunks = 0;
        while let Some(envelope) = observation.recv().await {
            assert!(
                !serde_json::to_string(&envelope)
                    .expect("json")
                    .contains("private-tool-output")
            );
            if envelope.event.name == "output_chunk" {
                chunks += 1;
                assert_eq!(
                    envelope.event.correlation.tool_call_id.as_deref(),
                    Some("call")
                );
                assert_eq!(envelope.event.data["byte_len"], bytes.len());
                assert!(!envelope.event.sensitive);
            }
        }
        assert_eq!(chunks, 1);
    }

    #[tokio::test]
    async fn dropped_process_observation_does_not_claim_cleanup() {
        let hub = bcode_xray::XrayHub::default();
        let mut observation = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: bcode_xray::XRAY_SCHEMA_VERSION,
                session_id: "session".into(),
                deep_capture: false,
            })
            .expect("observe");
        hub.context(bcode_xray::XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        })
        .scope(async {
            drop(ProcessObservation::new(ToolExecutionId(8)));
        })
        .await;
        observation.finalize().expect("cutoff");
        let mut unfinished = 0;
        while let Some(envelope) = observation.recv().await {
            if envelope.event.name == "unfinished" {
                unfinished += 1;
                assert_eq!(envelope.event.data["details"]["cleanup_confirmed"], false);
            }
            assert_ne!(envelope.event.name, "finished");
        }
        assert_eq!(unfinished, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn removed_environment_is_not_visible_to_child() {
        let runtime = ToolExecutionRuntime::new(1);
        let mut environment = BTreeMap::new();
        environment.insert("BCODE_PRIVATE_TEST".to_owned(), "private".to_owned());
        let result = runtime
            .run_process(ProcessExecutionRequest {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "printf %s \"${BCODE_PRIVATE_TEST-unset}\"".to_owned(),
                ],
                cwd: None,
                timeout: Some(Duration::from_secs(1)),
                max_output_bytes: 1024,
                inherit_environment: true,
                environment,
                remove_environment: vec!["BCODE_PRIVATE_TEST".to_owned()],
            })
            .await
            .expect("process returns output");
        assert_eq!(result.stdout.bytes, b"unset");
    }

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
                inherit_environment: true,
                environment: BTreeMap::new(),
                remove_environment: Vec::new(),
            })
            .await
            .expect("process returns timeout result");
        assert!(result.timed_out);
        assert!(!result.cancelled);
        let status = runtime.status().await;
        assert_eq!(status.running, 0);
        assert_eq!(status.completed, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn caller_cancellation_terminates_process_group_once() {
        let runtime = ToolExecutionRuntime::new(1);
        let cancel = runtime.cancellation_handle();
        let cancellation = cancel.clone();
        let task = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runtime
                    .run_process_cancellable(
                        ProcessExecutionRequest {
                            program: "sh".to_string(),
                            args: vec!["-c".to_string(), "sleep 5 & wait".to_string()],
                            cwd: None,
                            timeout: Some(Duration::from_secs(10)),
                            max_output_bytes: 1024,
                            inherit_environment: true,
                            environment: BTreeMap::new(),
                            remove_environment: Vec::new(),
                        },
                        &cancellation,
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        cancel.cancel();
        let result = task.await.expect("task").expect("cancelled result");
        assert!(result.cancelled);
        assert!(!result.timed_out);
        let status = runtime.status().await;
        assert_eq!(status.running, 0);
        assert_eq!(status.cancelled, 1);
        assert_eq!(status.completed, 0);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cancellation_terminates_windows_process_tree() {
        let marker_dir = tempfile::tempdir().expect("marker tempdir");
        let marker = marker_dir.path().join("descendant-finished.txt");
        let child_script = marker_dir.path().join("descendant.cmd");
        std::fs::write(
            &child_script,
            format!(
                "@ping -n 4 127.0.0.1 >NUL\r\n@echo escaped>\"{}\"\r\n",
                marker.display()
            ),
        )
        .expect("write descendant script");
        let parent_script = marker_dir.path().join("parent.cmd");
        std::fs::write(
            &parent_script,
            format!(
                "@start \"\" /B cmd.exe /D /C call \"{}\"\r\n@ping -n 30 127.0.0.1 >NUL\r\n",
                child_script.display()
            ),
        )
        .expect("write parent script");
        let runtime = Arc::new(ToolExecutionRuntime::new(1));
        let cancellation = runtime.cancellation_handle();
        let task_runtime = Arc::clone(&runtime);
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            task_runtime
                .run_process_cancellable(
                    ProcessExecutionRequest {
                        program: "cmd.exe".to_owned(),
                        args: vec![
                            "/D".to_owned(),
                            "/C".to_owned(),
                            "call".to_owned(),
                            parent_script.display().to_string(),
                        ],
                        cwd: None,
                        timeout: Some(Duration::from_secs(15)),
                        max_output_bytes: 1024,
                        inherit_environment: true,
                        environment: BTreeMap::new(),
                        remove_environment: Vec::new(),
                    },
                    &task_cancellation,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        cancellation.cancel();
        let result = task.await.expect("task").expect("cancelled result");
        assert!(result.cancelled);
        assert!(!result.timed_out);
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "cancelled descendant escaped the Windows Job Object"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_termination_distinguishes_exit_and_signal() {
        let runtime = ToolExecutionRuntime::new(1);
        let exited = runtime
            .run_process(ProcessExecutionRequest {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "exit 7".to_string()],
                cwd: None,
                timeout: Some(Duration::from_secs(1)),
                max_output_bytes: 1024,
                inherit_environment: true,
                environment: BTreeMap::new(),
                remove_environment: Vec::new(),
            })
            .await
            .expect("normal exit");
        assert_eq!(exited.termination, ProcessTermination::Exited { code: 7 });
        assert!(!exited.timed_out);
        assert!(!exited.cancelled);

        let signaled = runtime
            .run_process(ProcessExecutionRequest {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "kill -TERM $$".to_string()],
                cwd: None,
                timeout: Some(Duration::from_secs(1)),
                max_output_bytes: 1024,
                inherit_environment: true,
                environment: BTreeMap::new(),
                remove_environment: Vec::new(),
            })
            .await
            .expect("signal termination");
        assert_eq!(
            signaled.termination,
            ProcessTermination::Signaled { signal: SIGTERM }
        );
        assert!(!signaled.timed_out);
        assert!(!signaled.cancelled);
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
                inherit_environment: true,
                environment: BTreeMap::new(),
                remove_environment: Vec::new(),
            })
            .await
            .expect("process returns output");
        assert_eq!(result.stdout.bytes, b"abc");
        assert!(result.stdout.truncated);
        assert_eq!(result.stderr.bytes, b"");
        assert!(!result.stderr.truncated);
    }
}
