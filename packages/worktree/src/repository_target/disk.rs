//! Disk-backed complete exports; memory is bounded by tree metadata and copy buffers.
use super::{git, mode_matches, set_mode};
use bcode_plugin_sdk::ServiceCancellation;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(all(test, unix))]
mod tests;

/// Explicit rejection thresholds, never selection or truncation of a target.
#[derive(Debug, Clone, Copy)]
pub struct RepositoryExportLimits {
    /// Maximum complete blob bytes (disk budget, excluding metadata).
    pub max_bytes: u64,
    /// Maximum regular files.
    pub max_files: usize,
    /// Maximum encoded Git tree metadata bytes retained in memory.
    pub max_tree_bytes: usize,
    /// Maximum elapsed time for each Git blob read.
    pub blob_timeout: Duration,
}

impl Default for RepositoryExportLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024 * 1024,
            max_files: 100_000,
            max_tree_bytes: 16 * 1024 * 1024,
            blob_timeout: Duration::from_mins(2),
        }
    }
}

#[derive(Debug)]
struct Entry {
    path: String,
    mode: String,
    size: u64,
}

/// Complete immutable commit export backed by private temporary files, removed on drop.
/// No live checkout bytes are inputs. Callers must retain the streamed archive before drop.
///
/// Temporary storage and materialization destinations require exclusive caller ownership;
/// this API does not defend against concurrent writers with the same OS credentials.
#[derive(Debug)]
pub struct RepositorySpool {
    storage: tempfile::TempDir,
    commit: String,
    entries: Vec<Entry>,
    bytes: u64,
    identity: String,
}

fn cancelled(cancellation: &ServiceCancellation) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err("repository export cancelled".into())
    } else {
        Ok(())
    }
}

/// Export a complete commit using bounded metadata memory and disk-backed blob bytes.
/// Authorization must precede this operation (same facts as `authorization_command`).
/// # Errors
/// Rejects invalid identities, missing objects, unsafe/non-UTF-8 paths, symlinks,
/// submodules, LFS pointers, empty trees, exceeded limits, cancellation and I/O failures.
/// Partial temporary exports are removed on error; no complete identity is returned.
pub fn export_spooled(
    repository: &Path,
    commit: &str,
    limits: RepositoryExportLimits,
    cancellation: &ServiceCancellation,
) -> Result<RepositorySpool, String> {
    cancelled(cancellation)?;
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("repository target requires a full commit object identity".into());
    }
    if limits.max_tree_bytes == usize::MAX || limits.max_bytes == u64::MAX {
        return Err("repository export limits require overflow headroom".into());
    }
    if git(repository, &["cat-file", "-t", commit], 32, cancellation)? != b"commit\n" {
        return Err("repository target is not a commit".into());
    }
    let tree = git(
        repository,
        &["ls-tree", "-rz", "--full-tree", commit],
        limits.max_tree_bytes,
        cancellation,
    )?;
    let mut spool = RepositorySpool {
        storage: tempfile::tempdir().map_err(|_| "repository spool unavailable")?,
        commit: commit.into(),
        entries: Vec::new(),
        bytes: 0,
        identity: String::new(),
    };
    for record in tree
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        cancelled(cancellation)?;
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
        if spool.entries.len() >= limits.max_files {
            return Err("repository export exceeds explicit file limit".into());
        }
        let output = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(spool.storage.path().join(spool.entries.len().to_string()))
            .map_err(|_| "repository spool creation failed")?;
        let size = spool_blob(
            repository,
            fields[2],
            output,
            limits.max_bytes - spool.bytes,
            limits.blob_timeout,
            cancellation,
        )?;
        spool.bytes += size;
        spool.entries.push(Entry {
            path: path.into(),
            mode: fields[0].into(),
            size,
        });
    }
    if spool.entries.is_empty() {
        return Err("empty repository target".into());
    }
    let mut digest = DigestWriter(Sha256::new());
    spool.write_archive(&mut digest, cancellation)?;
    for byte in digest.0.finalize() {
        write!(&mut spool.identity, "{byte:02x}").expect("string formatting is infallible");
    }
    Ok(spool)
}

// The reader owns only one fixed buffer and one file. The supervising thread can
// kill blocked Git reads on cancellation/timeout; no whole blob crosses a channel.
fn spool_blob(
    repository: &Path,
    object: &str,
    mut output: std::fs::File,
    limit: u64,
    timeout: Duration,
    cancellation: &ServiceCancellation,
) -> Result<u64, String> {
    let mut child = Command::new("git")
        .args(["--no-replace-objects", "-C"])
        .arg(repository)
        .args(["cat-file", "blob", object])
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
    let mut stdout = child.stdout.take().ok_or("Git stdout unavailable")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let result = (|| -> Result<u64, String> {
            let size = std::io::copy(&mut stdout.by_ref().take(limit + 1), &mut output)
                .map_err(|_| "repository blob copy failed")?;
            if size > limit {
                return Err("repository export exceeds explicit byte limit".into());
            }
            output
                .seek(SeekFrom::Start(0))
                .map_err(|_| "repository spool seek failed")?;
            let mut prefix = [0; 64];
            let count = output
                .read(&mut prefix)
                .map_err(|_| "repository spool read failed")?;
            if prefix[..count].starts_with(b"version https://git-lfs.github.com/spec/v1\n")
                || prefix[..count].starts_with(b"version https://git-lfs.github.com/spec/v1\r\n")
            {
                return Err("Git LFS payload unavailable: pointer is not delivered content".into());
            }
            Ok(size)
        })();
        let _ = sender.send(result);
    });
    let started = Instant::now();
    let result = loop {
        if cancellation.is_cancelled() || started.elapsed() >= timeout {
            break Err("repository export cancelled or timed out".into());
        }
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => break result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break Err("repository blob reader failed".into()),
        }
    };
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    let _ = reader.join();
    let size = result?;
    if !status.is_ok_and(|status| status.success()) {
        return Err("repository Git read failed".into());
    }
    cancelled(cancellation)?;
    Ok(size)
}

struct DigestWriter(Sha256);
impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl RepositorySpool {
    /// Exact full commit object identity.
    #[must_use]
    pub fn commit(&self) -> &str {
        &self.commit
    }
    /// SHA-256 of the complete versioned archive, including paths, modes and bytes.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }
    /// Total complete blob bytes, not archive overhead.
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.bytes
    }
    /// Complete target file count.
    #[must_use]
    pub const fn file_count(&self) -> usize {
        self.entries.len()
    }

    /// Stream a deterministic complete binary archive. Format v1: magic
    /// `BCODE-REPOSITORY\0\x01`, then length-prefixed commit, u64 file count,
    /// and Git traversal-ordered entries (length-prefixed UTF-8 path, six ASCII
    /// mode bytes, u64 size, complete raw blob). All lengths are u64 big-endian.
    /// Unknown versions must be rejected. This is not the legacy JSON export.
    /// # Errors
    /// Cancellation or I/O failure leaves the caller's writer partial; publish only on success.
    pub fn write_archive(
        &self,
        output: &mut impl Write,
        cancellation: &ServiceCancellation,
    ) -> Result<(), String> {
        cancelled(cancellation)?;
        let result = (|| -> std::io::Result<()> {
            output.write_all(b"BCODE-REPOSITORY\0\x01")?;
            write_field(output, self.commit.as_bytes())?;
            output.write_all(&(self.entries.len() as u64).to_be_bytes())?;
            for (index, entry) in self.entries.iter().enumerate() {
                write_field(output, entry.path.as_bytes())?;
                output.write_all(entry.mode.as_bytes())?;
                output.write_all(&entry.size.to_be_bytes())?;
                let mut input = std::fs::File::open(self.storage.path().join(index.to_string()))?;
                copy_cancelled(&mut input, output, cancellation)?;
            }
            Ok(())
        })();
        cancelled(cancellation)?;
        result.map_err(|_| "repository archive write failed".into())
    }

    /// Materialize complete bytes and modes into an empty, private real directory.
    /// # Errors
    /// Rejects symlink roots, nonempty destinations, collisions, cancellation and I/O failure.
    /// A failure may leave partial files in the destination; callers own cleanup.
    pub fn materialize(
        &self,
        directory: &Path,
        cancellation: &ServiceCancellation,
    ) -> Result<(), String> {
        cancelled(cancellation)?;
        if !std::fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.is_dir())
            || std::fs::read_dir(directory)
                .map_err(|_| "target directory unavailable")?
                .next()
                .is_some()
        {
            return Err("repository target directory must be empty and not a symlink".into());
        }
        for (index, entry) in self.entries.iter().enumerate() {
            cancelled(cancellation)?;
            let path = directory.join(&entry.path);
            std::fs::create_dir_all(path.parent().ok_or("invalid file path")?)
                .map_err(|_| "directory creation failed")?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|_| "file creation failed")?;
            let mut input = std::fs::File::open(self.storage.path().join(index.to_string()))
                .map_err(|_| "repository spool unavailable")?;
            copy_cancelled(&mut input, &mut output, cancellation)
                .map_err(|_| "repository materialization failed or cancelled")?;
            set_mode(&path, &entry.mode)?;
        }
        cancelled(cancellation)
    }

    /// Compare complete delivered paths, modes and bytes with bounded memory.
    /// Extra build files are permitted; this is not hermetic execution proof.
    /// # Errors
    /// Cancellation is explicit; unreadable, redirected or changed files return false.
    pub fn matches(
        &self,
        directory: &Path,
        cancellation: &ServiceCancellation,
    ) -> Result<bool, String> {
        cancelled(cancellation)?;
        if !std::fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Ok(false);
        }
        for (index, entry) in self.entries.iter().enumerate() {
            cancelled(cancellation)?;
            let mut path = directory.to_path_buf();
            for component in entry.path.split('/') {
                path.push(component);
                if std::fs::symlink_metadata(&path).map_or(true, |m| m.file_type().is_symlink()) {
                    return Ok(false);
                }
            }
            if !std::fs::symlink_metadata(&path).is_ok_and(|m| {
                m.is_file() && m.len() == entry.size && mode_matches(&m, &entry.mode)
            }) {
                return Ok(false);
            }
            let Ok(mut actual) = std::fs::File::open(path) else {
                return Ok(false);
            };
            let mut expected = std::fs::File::open(self.storage.path().join(index.to_string()))
                .map_err(|_| "repository spool unavailable")?;
            let mut left = [0; 8192];
            let mut right = [0; 8192];
            loop {
                cancelled(cancellation)?;
                let count = expected
                    .read(&mut left)
                    .map_err(|_| "repository spool read failed")?;
                if count == 0 {
                    if actual.read(&mut right[..1]).ok() != Some(0) {
                        return Ok(false);
                    }
                    break;
                }
                if actual.read_exact(&mut right[..count]).is_err()
                    || left[..count] != right[..count]
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

fn write_field(output: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    output.write_all(&(bytes.len() as u64).to_be_bytes())?;
    output.write_all(bytes)
}

fn copy_cancelled(
    input: &mut impl Read,
    output: &mut impl Write,
    cancellation: &ServiceCancellation,
) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        if cancellation.is_cancelled() {
            return Err(std::io::Error::other("repository export cancelled"));
        }
        let count = input.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..count])?;
    }
}
