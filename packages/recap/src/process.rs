//! Bounded, cancellable argument-vector subprocess execution.
use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

type AdmissionSlot = Arc<std::sync::Mutex<Option<Arc<dyn ProcessAdmission>>>>;

/// Cooperative cancellation shared by acquisition and media processing.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>, AdmissionSlot);

/// Caller-injected authorization for each fully constructed literal process request.
pub trait ProcessAdmission: Send + Sync + std::fmt::Debug {
    /// Called before spawn. A failure must not start the process.
    /// # Errors
    /// Returns a secret-safe denial when operation policy or ownership cannot be verified.
    fn authorize(&self, request: &ProcessRequest) -> Result<(), ProcessError>;
}

impl Cancellation {
    /// Install a process admission capability before starting the pipeline.
    pub fn set_process_admission(&self, admission: Arc<dyn ProcessAdmission>) {
        *self
            .1
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(admission);
    }
    /// Request cancellation of current and subsequent operations.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
/// Explicit executable, arguments and resource limits. No shell is invoked.
#[derive(Clone, Debug)]
pub struct ProcessRequest {
    /// Executable path or explicitly selected PATH command.
    pub program: PathBuf,
    /// Literal arguments, not shell syntax.
    pub args: Vec<OsString>,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Wall-clock deadline.
    pub timeout: Duration,
    /// Maximum bytes retained from each output stream.
    pub max_output_bytes: usize,
    /// Optional flat owned workspace and aggregate on-disk byte budget, polled while running.
    pub file_budget: Option<(PathBuf, u64)>,
}
/// Successful bounded subprocess output.
#[derive(Debug)]
pub struct ProcessOutput {
    /// Standard output bytes.
    pub stdout: Vec<u8>,
    /// Standard error bytes.
    pub stderr: Vec<u8>,
}
/// Errors deliberately omit executable output, which can contain secrets.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// Explicit operation authorization failed before spawn.
    #[error("subprocess admission denied")]
    AdmissionDenied,
    /// Spawn, read, kill or reap failed.
    #[error("subprocess I/O failed")]
    Io(#[from] std::io::Error),
    /// Operation cancelled.
    #[error("subprocess cancelled")]
    Cancelled,
    /// Deadline exceeded.
    #[error("subprocess deadline exceeded")]
    Timeout,
    /// Untrusted output exceeded its budget.
    #[error("subprocess output exceeded budget")]
    OutputLimit,
    /// Nonzero or signal exit, without exposing raw stderr.
    #[error("subprocess failed (exit code {0:?})")]
    Failed(Option<i32>),
}
async fn read_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, ProcessError> {
    let mut result = Vec::new();
    let mut chunk = vec![0; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(result);
        }
        if count > limit.saturating_sub(result.len()) {
            return Err(ProcessError::OutputLimit);
        }
        result.extend_from_slice(&chunk[..count]);
    }
}
/// Run a process, concurrently drain bounded output, and kill/reap on failure.
///
/// # Errors
/// Returns an error on cancellation, deadline, output overflow, I/O or nonzero exit.
pub async fn run(
    request: &ProcessRequest,
    cancellation: &Cancellation,
) -> Result<ProcessOutput, ProcessError> {
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let admission = cancellation
        .1
        .lock()
        .map_err(|_| ProcessError::AdmissionDenied)?
        .clone();
    if let Some(admission) = admission {
        admission.authorize(request)?;
    }
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("missing stderr"))?;
    let result = {
        let work = async {
            let (status, stdout, stderr) = tokio::try_join!(
                async { child.wait().await.map_err(ProcessError::from) },
                read_bounded(stdout, request.max_output_bytes),
                read_bounded(stderr, request.max_output_bytes)
            )?;
            if !status.success() {
                return Err(ProcessError::Failed(status.code()));
            }
            Ok(ProcessOutput { stdout, stderr })
        };
        tokio::select! {
            result = work => result,
            () = tokio::time::sleep(request.timeout) => Err(ProcessError::Timeout),
            error = async {
                loop {
                    if let Some((root, limit)) = &request.file_budget {
                        let checked = (|| -> Result<(), ProcessError> {
                            let mut bytes = 0_u64;
                            for (index, entry) in std::fs::read_dir(root)?.enumerate() {
                                if index >= 128 { return Err(ProcessError::OutputLimit); }
                                let metadata = entry?.path().symlink_metadata()?;
                                if !metadata.is_file() || metadata.file_type().is_symlink() { return Err(ProcessError::OutputLimit); }
                                bytes = bytes.checked_add(metadata.len()).ok_or(ProcessError::OutputLimit)?;
                            }
                            if bytes > *limit { return Err(ProcessError::OutputLimit); }
                            Ok(())
                        })();
                        if let Err(error) = checked { break error; }
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            } => Err(error),
            () = async { while !cancellation.is_cancelled() { tokio::time::sleep(Duration::from_millis(25)).await; } } => Err(ProcessError::Cancelled),
        }
    };
    if result.is_err() {
        // Always wait even when kill reports that the process already exited.
        let _ = child.start_kill();
        child.wait().await?;
    }
    result
}

#[cfg(test)]
mod tests {
    #[derive(Debug)]
    struct DenyAdmission;
    impl super::ProcessAdmission for DenyAdmission {
        fn authorize(&self, _: &super::ProcessRequest) -> Result<(), super::ProcessError> {
            Err(super::ProcessError::AdmissionDenied)
        }
    }
    #[tokio::test]
    async fn admission_denial_precedes_spawn() {
        let cancel = super::Cancellation::default();
        cancel.set_process_admission(std::sync::Arc::new(DenyAdmission));
        let request = super::ProcessRequest {
            program: "/definitely/nonexistent-executable".into(),
            args: Vec::new(),
            cwd: None,
            timeout: std::time::Duration::from_secs(1),
            max_output_bytes: 1024,
            file_budget: None,
        };
        assert!(matches!(
            super::run(&request, &cancel).await,
            Err(super::ProcessError::AdmissionDenied)
        ));
    }
    use super::*;
    #[tokio::test]
    async fn cancellation_prevents_spawn() {
        let cancel = Cancellation::default();
        cancel.cancel();
        let request = ProcessRequest {
            program: "nonexistent-recap-test".into(),
            args: vec![],
            cwd: None,
            timeout: Duration::from_secs(1),
            max_output_bytes: 128,
            file_budget: None,
        };
        assert!(matches!(
            run(&request, &cancel).await,
            Err(ProcessError::Cancelled)
        ));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn limits_and_deadline_are_enforced() {
        let mut request = ProcessRequest {
            program: "/usr/bin/yes".into(),
            args: vec![],
            cwd: None,
            timeout: Duration::from_secs(1),
            max_output_bytes: 128,
            file_budget: None,
        };
        assert!(matches!(
            run(&request, &Cancellation::default()).await,
            Err(ProcessError::OutputLimit)
        ));
        request.program = "/bin/sleep".into();
        request.args = vec!["2".into()];
        request.timeout = Duration::from_millis(20);
        assert!(matches!(
            run(&request, &Cancellation::default()).await,
            Err(ProcessError::Timeout)
        ));
    }
}
