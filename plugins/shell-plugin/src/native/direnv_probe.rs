//! Bounded, content-free diagnostics for the host's direnv preflight.
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum Failure {
    Spawn(io::Error),
    Wait(io::Error),
    Exit(std::process::ExitStatus),
    Timeout,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(
                f,
                "spawn failed (kind={:?}, os_code={:?})",
                error.kind(),
                error.raw_os_error()
            ),
            Self::Wait(error) => write!(
                f,
                "wait failed (kind={:?}, os_code={:?})",
                error.kind(),
                error.raw_os_error()
            ),
            Self::Exit(status) => write!(f, "process failed ({status})"),
            Self::Timeout => f.write_str("timed out; direct probe process killed and reaped"),
        }
    }
}

fn run(command: &mut Command, timeout: Duration) -> Result<(), Failure> {
    // Never expose arbitrary executable output at the public tool-error boundary.
    // Null streams also avoid pipe backpressure and unbounded output buffering.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(Failure::Spawn)?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(Failure::Exit(status)),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                child.wait().map_err(Failure::Wait)?;
                return Err(Failure::Timeout);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Wait(error));
            }
        }
    }
}

fn bounded_path(path: &Path) -> String {
    // Escape controls and bound even attacker-controlled path names.
    path.to_string_lossy()
        .chars()
        .take(256)
        .flat_map(char::escape_default)
        .collect()
}

pub fn check(requested_cwd: Option<&Path>) -> Result<(), String> {
    let started = Instant::now();
    run(Command::new("direnv").arg("version"), PROBE_TIMEOUT).map_err(|failure| {
        let path = std::env::var_os("PATH");
        let fingerprint = path.as_ref().map_or_else(|| "unset".to_owned(), |value| {
            hex::encode(Sha256::digest(value.as_encoded_bytes()))
        });
        let entries = path.as_ref().map_or(0, |value| std::env::split_paths(value).count());
        let host_cwd = std::env::current_dir().map_or_else(|_| "unavailable".to_owned(), |path| bounded_path(&path));
        let requested = requested_cwd.map_or_else(|| "inherited".to_owned(), bounded_path);
        // A lookup hint, not a replacement resolver: preserve Command's launch semantics.
        let candidate = path.as_ref().and_then(|value| {
            std::env::split_paths(value).map(|dir| dir.join(OsStr::new("direnv")))
                .find(|candidate| candidate.symlink_metadata().is_ok())
        }).map_or_else(|| "none".to_owned(), |path| bounded_path(&path));
        format!("direnv preflight `direnv version`: {failure}; elapsed_ms={}; host_cwd={host_cwd}; requested_cwd={requested}; PATH_entries={entries}; PATH_sha256={fingerprint}; first_existing_candidate={candidate} (lookup hint only); stdout/stderr suppressed", started.elapsed().as_millis())
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn script(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }

    #[test]
    fn successful_probe() {
        let (_dir, path) = script("exit 0");
        assert!(run(&mut Command::new(path), PROBE_TIMEOUT).is_ok());
    }

    #[test]
    fn missing_and_broken_symlink_report_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let link = dir.path().join("broken");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        for path in [missing, link] {
            let failure = run(&mut Command::new(path), PROBE_TIMEOUT).unwrap_err();
            assert!(matches!(&failure, Failure::Spawn(e) if e.kind() == io::ErrorKind::NotFound));
            assert!(failure.to_string().contains("os_code=Some("));
        }
    }

    #[test]
    fn permission_error_is_not_reported_as_missing() {
        let (_dir, path) = script("exit 0");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            matches!(run(&mut Command::new(path), PROBE_TIMEOUT), Err(Failure::Spawn(e)) if e.kind() == io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn exit_and_signal_are_distinct_and_output_is_not_exposed() {
        use std::os::unix::process::ExitStatusExt as _;
        let (_dir, path) = script("echo PRIVATE_TOKEN >&2; exit 7");
        let failure = run(&mut Command::new(path), PROBE_TIMEOUT).unwrap_err();
        assert!(!failure.to_string().contains("PRIVATE_TOKEN"));
        assert!(matches!(failure, Failure::Exit(status) if status.code() == Some(7)));
        let (_dir, path) = script("kill -TERM $$");
        assert!(
            matches!(run(&mut Command::new(path), PROBE_TIMEOUT), Err(Failure::Exit(status)) if status.signal() == Some(15))
        );
    }

    #[test]
    fn hanging_probe_times_out_and_is_reaped() {
        let (_dir, path) = script("while :; do :; done");
        let started = Instant::now();
        assert!(matches!(
            run(&mut Command::new(path), Duration::from_millis(30)),
            Err(Failure::Timeout)
        ));
        assert!(started.elapsed() < PROBE_TIMEOUT);
    }

    #[test]
    fn path_diagnostics_escape_controls_and_bound_input() {
        let rendered = bounded_path(Path::new("/tmp/a\n\u{1b}[31m"));
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\u{1b}'));
        assert_eq!(bounded_path(Path::new(&"a".repeat(1_000))).len(), 256);
    }
}
