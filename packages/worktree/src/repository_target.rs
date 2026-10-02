//! Immutable repository export. This is not a snapshot of the working checkout.
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

mod disk;
pub use disk::{RepositoryExportLimits, RepositorySpool, export_spooled};

/// One complete regular Git blob, including its executable mode.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepositoryFile {
    /// Slash-separated UTF-8 repository-relative path.
    pub path: String,
    /// Git mode: 100644 or 100755.
    pub mode: String,
    /// Complete bytes, including binary content.
    pub bytes: Vec<u8>,
}

/// Complete supported tree at an exact commit. Limits are rejection thresholds,
/// never truncation or implicit exclusions.
#[derive(Debug, serde::Serialize)]
pub struct RepositoryExport {
    /// Export format version.
    pub version: u32,
    /// Full commit object identity, never a symbolic revision.
    pub commit: String,
    /// Complete regular-file tree.
    files: Vec<RepositoryFile>,
}

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 100_000;

/// Owner-produced read operation facts for shell authorization, not executable script.
/// The actual exporter uses argument arrays and validates the immutable identity.
#[must_use]
pub fn authorization_command(repository: &Path, commit: &str) -> String {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    format!(
        "git --no-replace-objects -C {} cat-file --batch ; git --no-replace-objects -C {} ls-tree -rz --full-tree {}",
        quote(&repository.to_string_lossy()),
        quote(&repository.to_string_lossy()),
        quote(commit)
    )
}

fn git(
    repository: &Path,
    arguments: &[&str],
    limit: usize,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
) -> Result<Vec<u8>, String> {
    if cancellation.is_cancelled() {
        return Err("repository export cancelled".into());
    }
    let mut child = Command::new("git")
        .args(["--no-replace-objects", "-C"])
        .arg(repository)
        .args(arguments)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "repository export: Git unavailable")?;
    let stdout = child.stdout.take().ok_or("Git stdout unavailable")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let started = std::time::Instant::now();
    let bytes = loop {
        if cancellation.is_cancelled() || started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("repository export cancelled or timed out".into());
        }
        match receiver.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(Ok(bytes)) if bytes.len() <= limit => break bytes,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(
                    "repository export unreadable or exceeds explicit content limit".into(),
                );
            }
        }
    };
    let _ = reader.join();
    if !child.wait().map_err(|_| "Git wait failed")?.success() {
        return Err("repository export: Git object unavailable".into());
    }
    Ok(bytes)
}

/// Export every regular blob at a full immutable commit, without checkout filters,
/// attributes, ignore rules, hooks, or changes to the index/worktree.
///
/// # Errors
/// Rejects invalid identities, unavailable objects, unsafe/non-UTF-8 paths, symlinks,
/// submodules, LFS pointers, and trees exceeding 100,000 files or 16 MiB of bytes.
/// Untracked/ignored/modified checkout files are not inputs: the requested commit
/// is the entire target, not a claim about checkout freshness.
pub fn export(
    repository: &Path,
    commit: &str,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
) -> Result<RepositoryExport, String> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("repository target requires a full commit object identity".into());
    }
    let kind = git(repository, &["cat-file", "-t", commit], 32, cancellation)?;
    if kind != b"commit\n" {
        return Err("repository target is not a commit".into());
    }
    let tree = git(
        repository,
        &["ls-tree", "-rz", "--full-tree", commit],
        MAX_BYTES,
        cancellation,
    )?;
    let mut files = Vec::new();
    let mut size = 0;
    for record in tree
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record).map_err(|_| "non-UTF-8 Git tree")?;
        let (metadata, path) = record.split_once('\t').ok_or("invalid Git tree")?;
        let fields: Vec<_> = metadata.split(' ').collect();
        if fields.len() != 3 || fields[1] != "blob" || !matches!(fields[0], "100644" | "100755") {
            return Err(
                "unsupported Git tree entry: symlinks and submodules are not delivered".into(),
            );
        }
        if path.is_empty()
            || path.contains('\\')
            || path.split('/').any(|part| {
                part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git")
            })
        {
            return Err("unsafe repository path".into());
        }
        if files.len() == MAX_FILES {
            return Err("repository export exceeds explicit file limit".into());
        }
        let bytes = git(
            repository,
            &["cat-file", "blob", fields[2]],
            MAX_BYTES - size,
            cancellation,
        )?;
        if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n")
            || bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\r\n")
        {
            return Err("Git LFS payload unavailable: pointer is not delivered content".into());
        }
        size += bytes.len();
        files.push(RepositoryFile {
            path: path.into(),
            mode: fields[0].into(),
            bytes,
        });
    }
    if files.is_empty() {
        return Err("empty repository target".into());
    }
    Ok(RepositoryExport {
        version: 1,
        commit: commit.to_ascii_lowercase(),
        files,
    })
}

impl RepositoryExport {
    /// Materialize exact blob bytes and modes in an empty private directory.
    /// # Errors
    /// Rejects nonempty destinations, path collisions, unsupported platforms or I/O failure.
    pub fn materialize(&self, directory: &Path) -> Result<(), String> {
        if std::fs::read_dir(directory)
            .map_err(|_| "target directory unavailable")?
            .next()
            .is_some()
        {
            return Err("repository target directory must be empty".into());
        }
        for file in &self.files {
            let path = directory.join(&file.path);
            std::fs::create_dir_all(path.parent().ok_or("invalid file path")?)
                .map_err(|_| "directory creation failed")?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|_| "repository materialization collision or I/O failure")?;
            output
                .write_all(&file.bytes)
                .map_err(|_| "repository materialization failed")?;
            set_mode(&path, &file.mode)?;
        }
        Ok(())
    }

    /// Compare all source bytes and modes after a check. Newly generated build files
    /// are permitted; they are not delivered. This does not establish hermetic execution.
    #[must_use]
    pub fn matches(&self, directory: &Path) -> bool {
        self.files.iter().all(|file| {
            let path = directory.join(&file.path);
            // Check every ancestor to reject command-created symlink redirection.
            let mut current = directory.to_path_buf();
            for component in file.path.split('/') {
                current.push(component);
                if std::fs::symlink_metadata(&current)
                    .map_or(true, |metadata| metadata.file_type().is_symlink())
                {
                    return false;
                }
            }
            std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.is_file() && mode_matches(&metadata, &file.mode))
                && std::fs::File::open(&path).is_ok_and(|input| content_matches(input, &file.bytes))
        })
    }
}

// Compare exact bytes with fixed additional memory, including an EOF check so
// appended content cannot be mistaken for unchanged source.
fn content_matches(mut input: impl Read, expected: &[u8]) -> bool {
    let mut buffer = [0_u8; 8192];
    for chunk in expected.chunks(buffer.len()) {
        let actual = &mut buffer[..chunk.len()];
        if input.read_exact(actual).is_err() || actual != chunk {
            return false;
        }
    }
    loop {
        match input.read(&mut buffer[..1]) {
            Ok(0) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            _ => return false,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn exact_content_comparison_handles_boundaries_and_rejects_mutations() {
        for length in [0, 1, 8191, 8192, 8193, 32_769] {
            let expected = vec![42; length];
            assert!(content_matches(expected.as_slice(), &expected));
            let mut changed = expected.clone();
            changed.push(42);
            assert!(!content_matches(changed.as_slice(), &expected));
            if length > 0 {
                assert!(!content_matches(&expected[..length - 1], &expected));
                changed.truncate(length);
                changed[length - 1] ^= 1;
                assert!(!content_matches(changed.as_slice(), &expected));
            }
        }
    }

    fn run(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
        String::from_utf8_lossy(&output.stdout).trim().into()
    }
    #[test]
    fn rejects_lfs_pointers_with_lf_or_crlf_without_delivering_pointer_bytes() {
        for newline in ["\n", "\r\n"] {
            let source = tempfile::tempdir().unwrap();
            run(source.path(), &["init", "-q"]);
            let pointer = format!(
                "version https://git-lfs.github.com/spec/v1{newline}oid sha256:{}{newline}size 123{newline}",
                "a".repeat(64)
            );
            std::fs::write(source.path().join("asset"), &pointer).unwrap();
            run(source.path(), &["add", "asset"]);
            run(
                source.path(),
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.test",
                    "commit",
                    "-qm",
                    "pointer",
                ],
            );
            let error = export(
                source.path(),
                &run(source.path(), &["rev-parse", "HEAD"]),
                &bcode_plugin_sdk::ServiceCancellation::default(),
            )
            .unwrap_err();
            assert!(error.contains("Git LFS payload unavailable"));
            assert_eq!(
                std::fs::read_to_string(source.path().join("asset")).unwrap(),
                pointer
            );
        }
    }

    #[test]
    fn complete_binary_modes_dirty_checkout_and_unsupported_content() {
        let source = tempfile::tempdir().unwrap();
        run(source.path(), &["init", "-q"]);
        std::fs::write(source.path().join("binary"), [0, 255, 1]).unwrap();
        std::fs::write(source.path().join("run"), b"#!/bin/sh\nexit 0\n").unwrap();
        set_mode(&source.path().join("run"), "100755").unwrap();
        run(source.path(), &["add", "."]);
        run(
            source.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let commit = run(source.path(), &["rev-parse", "HEAD"]);
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        // The owning capability must enforce the same identity representation as
        // its public callers, rather than silently normalize unsupported input.
        for invalid in ["HEAD".to_owned(), "A".repeat(40), "0".repeat(40)] {
            assert!(export(source.path(), &invalid, &cancellation).is_err());
        }
        let blob = run(source.path(), &["rev-parse", "HEAD:binary"]);
        assert!(export(source.path(), &blob, &cancellation).is_err());
        std::fs::write(source.path().join("binary"), b"dirty user work").unwrap();
        std::fs::write(source.path().join("untracked"), b"user work").unwrap();
        let target = export(
            source.path(),
            &commit,
            &bcode_plugin_sdk::ServiceCancellation::default(),
        )
        .unwrap();
        assert_eq!(target.files.len(), 2);
        let destination = tempfile::tempdir().unwrap();
        target.materialize(destination.path()).unwrap();
        assert_eq!(
            std::fs::read(destination.path().join("binary")).unwrap(),
            [0, 255, 1]
        );
        assert!(target.matches(destination.path()));
        std::fs::write(destination.path().join("generated"), b"build output").unwrap();
        assert!(target.matches(destination.path()));
        set_mode(&destination.path().join("run"), "100644").unwrap();
        assert!(!target.matches(destination.path()));
        assert!(target.materialize(destination.path()).is_err());
        assert_eq!(
            std::fs::read(source.path().join("binary")).unwrap(),
            b"dirty user work"
        );
        assert!(
            export(
                source.path(),
                "HEAD",
                &bcode_plugin_sdk::ServiceCancellation::default()
            )
            .is_err()
        );
        std::os::unix::fs::symlink("binary", source.path().join("link")).unwrap();
        run(source.path(), &["add", "link"]);
        run(
            source.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-qm",
                "unsupported",
            ],
        );
        assert!(
            export(
                source.path(),
                &run(source.path(), &["rev-parse", "HEAD"]),
                &bcode_plugin_sdk::ServiceCancellation::default()
            )
            .is_err()
        );
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(if mode == "100755" { 0o755 } else { 0o644 }),
    )
    .map_err(|_| "repository mode materialization failed".into())
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: &str) -> Result<(), String> {
    Err("repository target modes unsupported on this platform".into())
}
#[cfg(unix)]
fn mode_matches(metadata: &std::fs::Metadata, mode: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777 == if mode == "100755" { 0o755 } else { 0o644 }
}
#[cfg(not(unix))]
fn mode_matches(_metadata: &std::fs::Metadata, _mode: &str) -> bool {
    false
}
