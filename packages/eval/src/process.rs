//! Deadline-enforced child execution with file-backed, bounded observations.
use super::{Command, EvalError, Instant, Path, Stdio, fs};
type CapturedProcess = (Option<i32>, bool, Vec<u8>, Vec<u8>);
use std::time::Duration;

pub fn bounded_command(
    command: Command,
    timeout: Duration,
    directory: &Path,
    label: &str,
) -> Result<CapturedProcess, EvalError> {
    bounded_command_policy(command, timeout, None, false, directory, label)
}

/// Execute with separate inactivity and optional total-duration limits.
/// Command output growth is the only observable progress for opaque subprocesses.
pub fn bounded_command_policy(
    mut command: Command,
    timeout: Duration,
    max_duration: Option<Duration>,
    idle: bool,
    directory: &Path,
    label: &str,
) -> Result<CapturedProcess, EvalError> {
    let stdout = directory.join(format!("{label}.stdout"));
    let stderr = directory.join(format!("{label}.stderr"));
    command
        .stdout(Stdio::from(fs::File::create(&stdout)?))
        .stderr(Stdio::from(fs::File::create(&stderr)?));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let started = Instant::now();
    let mut last_progress = started;
    let mut observed_bytes = 0;
    let (code, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status.code().unwrap_or(-1)), false);
        }
        if idle {
            let bytes = fs::metadata(&stdout)?
                .len()
                .saturating_add(fs::metadata(&stderr)?.len());
            if bytes > observed_bytes {
                observed_bytes = bytes;
                last_progress = Instant::now();
            }
        }
        if (if idle {
            last_progress.elapsed()
        } else {
            started.elapsed()
        }) >= timeout
            || max_duration.is_some_and(|limit| started.elapsed() >= limit)
        {
            #[cfg(unix)]
            {
                let group = format!("-{}", child.id());
                let status = Command::new("/bin/kill")
                    .args(["-KILL", "--", &group])
                    .status()?;
                if !status.success() && child.try_wait()?.is_none() {
                    child.kill()?;
                }
            }
            #[cfg(not(unix))]
            child.kill()?;
            child.wait()?;
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let read = |path: &Path| -> Result<Vec<u8>, EvalError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(1_048_577)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1_048_576 {
            return Err(EvalError::Validation(
                "child output exceeds bounded capture; full files retained".into(),
            ));
        }
        Ok(bytes)
    };
    Ok((code, timed_out, read(&stdout)?, read(&stderr)?))
}

#[cfg(test)]
mod tests {
    use super::bounded_command;
    use std::{process::Command, time::Duration};
    #[test]
    fn subprocess_progress_extends_idle_but_not_explicit_total_limit() {
        let directory = tempfile::tempdir().unwrap();
        let run = |total, label| {
            let mut command = Command::new("sh");
            command.args([
                "-c",
                "i=0; while [ $i -lt 6 ]; do printf output; sleep 0.05; i=$((i+1)); done",
            ]);
            super::bounded_command_policy(
                command,
                Duration::from_millis(200),
                total,
                true,
                directory.path(),
                label,
            )
            .unwrap()
        };
        let (code, expired, _, _) = run(None, "active");
        assert_eq!(code, Some(0));
        assert!(!expired);
        let (code, expired, _, _) = run(Some(Duration::from_millis(100)), "capped");
        assert_eq!(code, None);
        assert!(expired);
    }

    #[test]
    fn a_child_deadline_is_observed_and_output_retained() {
        let directory =
            std::env::temp_dir().join(format!("bcode-process-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut command = Command::new("sh");
        command.args(["-c", "printf started; sleep 10"]);
        let (code, expired, output, _) =
            bounded_command(command, Duration::from_millis(50), &directory, "test").unwrap();
        assert!(expired);
        assert!(code.is_none());
        assert_eq!(output, b"started");
        std::fs::remove_file(directory.join("test.stdout")).unwrap();
        std::fs::remove_file(directory.join("test.stderr")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
