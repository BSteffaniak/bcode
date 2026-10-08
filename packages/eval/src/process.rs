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

/// Execute an opaque command with wall-clock and optional total-duration limits.
///
/// Idle mode is unsupported without a typed substantive-progress contract. Output
/// bytes, including heartbeat output, cannot establish meaningful activity.
/// Rejection precedes output-file creation and process spawning.
pub fn bounded_command_policy(
    mut command: Command,
    timeout: Duration,
    max_duration: Option<Duration>,
    idle: bool,
    directory: &Path,
    label: &str,
) -> Result<CapturedProcess, EvalError> {
    if idle {
        return Err(EvalError::Validation(
            "opaque command idle timeout unsupported: no typed substantive progress channel".into(),
        ));
    }
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
    let (code, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status.code().unwrap_or(-1)), false);
        }
        if started.elapsed() >= timeout
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
    fn opaque_idle_policy_is_rejected_before_spawn_or_capture() {
        let directory = tempfile::tempdir().unwrap();
        for total in [None, Some(Duration::from_millis(100))] {
            // A nonexistent executable proves rejection happens before spawning;
            // no heartbeat or stdout protocol is implicitly accepted.
            let command = Command::new(directory.path().join("must-not-spawn"));
            let error = super::bounded_command_policy(
                command,
                Duration::from_millis(200),
                total,
                true,
                directory.path(),
                "unsupported",
            )
            .unwrap_err();
            assert!(matches!(error, super::EvalError::Validation(message)
                if message == "opaque command idle timeout unsupported: no typed substantive progress channel"));
            assert!(!directory.path().join("unsupported.stdout").exists());
            assert!(!directory.path().join("unsupported.stderr").exists());
        }
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
