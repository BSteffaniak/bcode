//! Blob line ranges (`review.blob.lines.get`).
//!
//! Object reads go through one long-lived `git cat-file --batch` process per repository root,
//! held in a bounded registry: idle processes are stopped (checked on every data layer call),
//! the least recently used one is evicted when the registry is full, a process that fails
//! mid-request is restarted once, and a process that reports an object above the size bound is
//! stopped rather than drained.
//! Working-tree reads come from disk with path confinement.

use super::{ServiceError, git};
use bcode_code_review_models::{
    REVIEW_BLOB_LINES_MAX_COUNT, ReviewBlobLinesRequest, ReviewBlobLinesResponse,
    ReviewBlobSelector, RevisionRef,
};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default maximum concurrently running `cat-file` processes.
const DEFAULT_MAX_PROCESSES: usize = 4;
/// Default idle time after which a `cat-file` process is stopped.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest blob the service reads.
const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
/// Byte budget of the decoded blob cache.
const BLOB_CACHE_BYTES: usize = 32 * 1024 * 1024;
/// Bytes inspected for NUL when classifying content as binary (git's heuristic).
const BINARY_SNIFF_BYTES: usize = 8_000;

/// One running `git cat-file --batch` process.
struct CatFileProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    last_used: Instant,
}

/// One `cat-file --batch` answer: `None` when the object is missing; otherwise its header and
/// either its content or, above the size bound, its size.
type ObjectRead = Option<(ObjectHeader, Result<Vec<u8>, u64>)>;

/// Header of one object returned by `cat-file --batch`.
struct ObjectHeader {
    oid: String,
    kind: String,
}

impl CatFileProcess {
    fn spawn(root: &Path) -> Result<Self, ServiceError> {
        let mut child = git::command(root, &["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ServiceError::Git("cat-file stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ServiceError::Git("cat-file stdout unavailable".to_string()))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            last_used: Instant::now(),
        })
    }

    /// Read one object. `Ok(None)` means git reported it missing. Protocol errors are I/O
    /// errors so the registry restarts the process.
    fn read(&mut self, spec: &str) -> std::io::Result<ObjectRead> {
        self.last_used = Instant::now();
        self.stdin.write_all(spec.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut header = String::new();
        if self.stdout.read_line(&mut header)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "cat-file exited",
            ));
        }
        let header = header.trim_end_matches('\n');
        if header.ends_with(" missing") || header.ends_with(" ambiguous") {
            return Ok(None);
        }
        let mut fields = header.rsplitn(3, ' ');
        let (Some(size), Some(kind), Some(oid)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(std::io::Error::other(format!(
                "unexpected cat-file header: {header}"
            )));
        };
        let size: u64 = size
            .parse()
            .map_err(|_| std::io::Error::other(format!("unexpected cat-file header: {header}")))?;
        let header = ObjectHeader {
            oid: oid.to_string(),
            kind: kind.to_string(),
        };
        if size > MAX_BLOB_BYTES {
            // The unread content leaves the stream misaligned; the registry stops this process
            // instead of draining an arbitrarily large object.
            return Ok(Some((header, Err(size))));
        }
        let mut content = vec![0; usize::try_from(size).unwrap_or(usize::MAX)];
        self.stdout.read_exact(&mut content)?;
        let mut terminator = [0_u8; 1];
        self.stdout.read_exact(&mut terminator)?;
        Ok(Some((header, Ok(content))))
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for CatFileProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Bounded registry of `cat-file --batch` processes keyed by repository root.
pub struct CatFileRegistry {
    processes: BTreeMap<PathBuf, CatFileProcess>,
    max_processes: usize,
    idle_timeout: Duration,
    spawned: u64,
}

impl Default for CatFileRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PROCESSES, DEFAULT_IDLE_TIMEOUT)
    }
}

impl CatFileRegistry {
    /// Create a registry holding at most `max_processes` processes, each stopped after
    /// `idle_timeout` without use.
    #[must_use]
    pub fn new(max_processes: usize, idle_timeout: Duration) -> Self {
        Self {
            processes: BTreeMap::new(),
            max_processes: max_processes.max(1),
            idle_timeout,
            spawned: 0,
        }
    }

    /// Processes started since the registry was created.
    #[must_use]
    pub const fn spawned(&self) -> u64 {
        self.spawned
    }

    /// Process id of the running process for `root`, if any.
    #[must_use]
    pub fn pid(&self, root: &Path) -> Option<u32> {
        self.processes.get(root).map(CatFileProcess::pid)
    }

    /// Number of running processes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    /// Whether no process is running.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }

    /// Stop processes idle for longer than the idle timeout.
    pub fn evict_idle(&mut self) {
        let timeout = self.idle_timeout;
        self.processes
            .retain(|_, process| process.last_used.elapsed() <= timeout);
    }

    /// Kill the process for `root` without removing it, simulating a crash (tests only).
    #[cfg(test)]
    pub fn kill_for_test(&mut self, root: &Path) {
        if let Some(process) = self.processes.get_mut(root) {
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }

    /// Read one object through the process for `root`, restarting it once on failure.
    fn read(&mut self, root: &Path, spec: &str) -> Result<ObjectRead, ServiceError> {
        self.evict_idle();
        let mut last_error = None;
        for _ in 0..2 {
            let process = self.process(root)?;
            match process.read(spec) {
                Ok(result) => {
                    if matches!(result, Some((_, Err(_)))) {
                        self.processes.remove(root);
                    }
                    return Ok(result);
                }
                Err(error) => {
                    self.processes.remove(root);
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.map_or_else(
            || ServiceError::Git("cat-file failed".to_string()),
            ServiceError::Io,
        ))
    }

    fn process(&mut self, root: &Path) -> Result<&mut CatFileProcess, ServiceError> {
        if !self.processes.contains_key(root) {
            while self.processes.len() >= self.max_processes {
                let oldest = self
                    .processes
                    .iter()
                    .min_by_key(|(_, process)| process.last_used)
                    .map(|(root, _)| root.clone());
                let Some(oldest) = oldest else {
                    break;
                };
                self.processes.remove(&oldest);
            }
            let process = CatFileProcess::spawn(root)?;
            self.spawned += 1;
            self.processes.insert(root.to_path_buf(), process);
        }
        self.processes
            .get_mut(root)
            .ok_or_else(|| ServiceError::Git("cat-file process unavailable".to_string()))
    }
}

/// Decoded blob content with line offsets.
struct BlobText {
    bytes: Vec<u8>,
    /// Start offset of every line.
    line_starts: Vec<usize>,
    binary: bool,
}

impl BlobText {
    fn new(bytes: Vec<u8>) -> Self {
        let binary = bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0);
        let mut line_starts = Vec::new();
        if !binary && !bytes.is_empty() {
            line_starts.push(0);
            for (index, byte) in bytes.iter().enumerate() {
                if *byte == b'\n' && index + 1 < bytes.len() {
                    line_starts.push(index + 1);
                }
            }
        }
        Self {
            bytes,
            line_starts,
            binary,
        }
    }

    fn lines(&self, start_line: u32, count: u32) -> ReviewBlobLinesResponse {
        let total = self.line_starts.len();
        let first = usize::try_from(start_line.max(1) - 1).unwrap_or(usize::MAX);
        let count = usize::try_from(count.min(REVIEW_BLOB_LINES_MAX_COUNT)).unwrap_or(0);
        let lines = (first..total.min(first.saturating_add(count)))
            .map(|index| {
                let start = self.line_starts[index];
                let end = self
                    .line_starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(self.bytes.len());
                let line = &self.bytes[start..end];
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                String::from_utf8_lossy(line).into_owned()
            })
            .collect();
        ReviewBlobLinesResponse {
            start_line: start_line.max(1),
            lines,
            total_lines: u32::try_from(total).unwrap_or(u32::MAX),
            binary: self.binary,
        }
    }
}

/// Blob reader: the `cat-file` registry plus a byte-budgeted cache of decoded blobs by id.
#[derive(Default)]
pub struct BlobService {
    registry: CatFileRegistry,
    cache: VecDeque<(String, Arc<BlobText>)>,
    cached_bytes: usize,
}

impl BlobService {
    /// The `cat-file` process registry.
    #[must_use]
    pub const fn registry(&self) -> &CatFileRegistry {
        &self.registry
    }

    /// Mutable access to the `cat-file` process registry.
    pub const fn registry_mut(&mut self) -> &mut CatFileRegistry {
        &mut self.registry
    }

    /// Drop every cached blob (tests use this to force object reads).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.cached_bytes = 0;
    }

    /// Return the requested line range.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::NotFound`] when the object or path does not exist or is not a blob.
    /// * [`ServiceError::PathRejected`] for unconfined paths.
    /// * [`ServiceError::TooLarge`] for blobs above the size bound.
    /// * [`ServiceError::Git`] / [`ServiceError::Io`] when reading fails.
    pub fn lines(
        &mut self,
        root: &Path,
        request: &ReviewBlobLinesRequest,
    ) -> Result<ReviewBlobLinesResponse, ServiceError> {
        let text = match &request.blob {
            ReviewBlobSelector::Object { oid } => {
                git::validate_oid(oid)?;
                // Cached blobs are keyed by full object id; an abbreviated id goes through
                // `cat-file`, which resolves it (or reports it ambiguous) before the cache is used.
                match self.cached(oid) {
                    Some(text) => text,
                    None => self.read_object(root, oid, oid)?,
                }
            }
            ReviewBlobSelector::Revision { revision, path } => {
                git::validate_relative_path(path)?;
                match revision {
                    RevisionRef::Commit { oid } => {
                        git::validate_oid(oid)?;
                        self.read_object(root, &format!("{oid}:{path}"), path)?
                    }
                    // An explicit stage keeps paths such as `1:x` from being read as a stage.
                    RevisionRef::Index => self.read_object(root, &format!(":0:{path}"), path)?,
                    RevisionRef::WorkingTree => {
                        Arc::new(BlobText::new(read_working_tree(root, path)?))
                    }
                    RevisionRef::EmptyTree => {
                        return Err(ServiceError::NotFound(format!(
                            "{path} does not exist in the empty tree"
                        )));
                    }
                }
            }
        };
        Ok(text.lines(request.start_line, request.count))
    }

    fn cached(&mut self, oid: &str) -> Option<Arc<BlobText>> {
        let position = self.cache.iter().position(|(cached, _)| cached == oid)?;
        let entry = self.cache.remove(position)?;
        let text = Arc::clone(&entry.1);
        self.cache.push_back(entry);
        Some(text)
    }

    fn read_object(
        &mut self,
        root: &Path,
        spec: &str,
        label: &str,
    ) -> Result<Arc<BlobText>, ServiceError> {
        let Some((header, content)) = self.registry.read(root, spec)? else {
            return Err(ServiceError::NotFound(format!("{label} not found")));
        };
        if header.kind != "blob" {
            return Err(ServiceError::NotFound(format!(
                "{label} is a {}, not a blob",
                header.kind
            )));
        }
        let content = content.map_err(|size| {
            ServiceError::TooLarge(format!("{label} is {size} bytes (limit {MAX_BLOB_BYTES})"))
        })?;
        if let Some(text) = self.cached(&header.oid) {
            return Ok(text);
        }
        let text = Arc::new(BlobText::new(content));
        self.insert(header.oid, Arc::clone(&text));
        Ok(text)
    }

    fn insert(&mut self, oid: String, text: Arc<BlobText>) {
        let size = text.bytes.len() + text.line_starts.len() * std::mem::size_of::<usize>();
        if size > BLOB_CACHE_BYTES {
            return;
        }
        while self.cached_bytes + size > BLOB_CACHE_BYTES {
            let Some((_, evicted)) = self.cache.pop_front() else {
                break;
            };
            self.cached_bytes = self.cached_bytes.saturating_sub(
                evicted.bytes.len() + evicted.line_starts.len() * std::mem::size_of::<usize>(),
            );
        }
        self.cached_bytes += size;
        self.cache.push_back((oid, text));
    }
}

/// Read working-tree content of a confined repository-relative path.
///
/// A symbolic link reads as its target path, matching git's blob content for links. Otherwise
/// the path is canonicalized and must stay inside the canonical repository root, so links in
/// parent directories cannot escape it.
fn read_working_tree(root: &Path, path: &str) -> Result<Vec<u8>, ServiceError> {
    git::validate_relative_path(path)?;
    let root = std::fs::canonicalize(root)?;
    let full = root.join(path);
    let rejected =
        || ServiceError::PathRejected(format!("{path:?} resolves outside the repository"));
    let metadata = std::fs::symlink_metadata(&full).map_err(|_| {
        ServiceError::NotFound(format!("{path} does not exist in the working tree"))
    })?;
    let parent = full
        .parent()
        .map(std::fs::canonicalize)
        .transpose()?
        .ok_or_else(rejected)?;
    if !parent.starts_with(&root) {
        return Err(rejected());
    }
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&full)?;
        return Ok(target.to_string_lossy().into_owned().into_bytes());
    }
    let canonical = std::fs::canonicalize(&full)?;
    if !canonical.starts_with(&root) {
        return Err(rejected());
    }
    if !metadata.is_file() {
        return Err(ServiceError::NotFound(format!(
            "{path} is not a regular file"
        )));
    }
    if metadata.len() > MAX_BLOB_BYTES {
        return Err(ServiceError::TooLarge(format!(
            "{path} is {} bytes (limit {MAX_BLOB_BYTES})",
            metadata.len()
        )));
    }
    Ok(std::fs::read(canonical)?)
}
