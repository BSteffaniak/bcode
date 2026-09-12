//! Bounded, crash-detecting access metadata, separate from canonical session history.
//!
//! The caller owns path confinement, file creation, and scheduling. An empty file is unknown,
//! never proof of inactivity. This store does not publish session events or use filesystem atime.

use bcode_session_models::SessionId;
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"BCACCESS";
const VERSION: u64 = 1;
const BYTES: usize = 64;
// Round forward, never backward: coalescing may delay compression by at most one minute.
const ACCESS_WINDOW_MS: u64 = 60_000;

const fn conservative_access_time(now_ms: u64) -> u64 {
    let remainder = now_ms % ACCESS_WINDOW_MS;
    if remainder == 0 {
        now_ms
    } else {
        now_ms.saturating_add(ACCESS_WINDOW_MS - remainder)
    }
}

/// Successful application reads that influence storage temperature.
#[derive(Debug, Clone, Copy)]
pub enum StorageAccessKind {
    /// Explicit user history navigation, inspection, attach, or export.
    History,
    /// Reading original artifact content (including terminal seeking).
    Artifact,
    /// Model-context construction is real consumption, unlike background index ingestion.
    ModelContext,
}

/// A versioned access record. `observed_at_ms` never decreases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageAccessRecord {
    /// Conservative upper bound on latest meaningful access or initialization, in Unix ms.
    /// Session-level recording rounds forward to coalesce writes, never making content look older.
    pub observed_at_ms: u64,
    /// Number of committed updates. Overflow fails closed rather than reusing a generation.
    pub generation: u64,
}

/// Result of bounded access observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageAccessObservation {
    /// No tracking has been initialized. Maintenance must not infer old age.
    Unknown,
    /// A complete current record.
    Recorded(StorageAccessRecord),
}

/// Record access in the owning session directory without creating canonical session storage.
///
/// The metadata is optional and separate from canonical events. Callers must surface failures to
/// the tiering coordinator; an error is not evidence that the previous timestamp is still valid.
/// Only call after a successful meaningful read. This does not acquire maintenance authority.
/// Timestamps round forward to one-minute boundaries. Reads within one boundary share a durable
/// update; the rounding can only postpone eligibility, never accelerate it. No queued timestamp
/// or in-memory cache is trusted instead of the locked record.
///
/// # Errors
///
/// Returns an error for missing canonical storage, unsafe paths, nonregular metadata, contention,
/// corrupt/future state, or I/O. On unsupported platforms it fails closed without creating files.
pub fn record_session_access(
    root: &Path,
    session_id: SessionId,
    kind: StorageAccessKind,
    now_ms: u64,
) -> io::Result<StorageAccessRecord> {
    let root = root.canonicalize()?;
    let (mut file, directory_handle) = open_session_access_file(&root, session_id)?;
    let result = record_access(&mut file, kind, conservative_access_time(now_ms))?;
    directory_handle.sync_all()?;
    Ok(result)
}

#[cfg(unix)]
fn open_session_access_file(root: &Path, session_id: SessionId) -> io::Result<(File, File)> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::MetadataExt as _;
    fn child(parent: &File, name: &std::ffi::CStr, flags: i32) -> io::Result<File> {
        // SAFETY: parent is an owned valid descriptor, name is NUL-terminated, and the returned
        // descriptor is transferred exactly once into File. All traversal is relative to handles.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh descriptor owned by this function.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    let root = File::open(root)?;
    let name = CString::new(session_id.to_string()).map_err(|_| invalid())?;
    let directory = child(&root, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let canonical = child(&directory, c"session.db", libc::O_RDONLY)?;
    if !canonical.metadata()?.is_file() {
        return Err(invalid());
    }
    let file = child(
        &directory,
        c"storage-access.bin",
        libc::O_RDWR | libc::O_CREAT,
    )?;
    if !file.metadata()?.is_file() || file.metadata()?.nlink() != 1 {
        return Err(invalid());
    }
    Ok((file, directory))
}

#[cfg(not(unix))]
fn open_session_access_file(_root: &Path, _session_id: SessionId) -> io::Result<(File, File)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "confined storage access tracking is unavailable on this platform",
    ))
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "storage access metadata requires maintenance",
    )
}

fn decode(bytes: &[u8; BYTES]) -> io::Result<StorageAccessRecord> {
    if &bytes[..8] != MAGIC || bytes[32..] != Sha256::digest(&bytes[..32])[..] {
        return Err(invalid());
    }
    if bytes[8..16] != VERSION.to_le_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported storage access metadata version",
        ));
    }
    let observed_at_ms = u64::from_le_bytes(bytes[16..24].try_into().map_err(|_| invalid())?);
    let generation = u64::from_le_bytes(bytes[24..32].try_into().map_err(|_| invalid())?);
    if generation == 0 {
        return Err(invalid());
    }
    Ok(StorageAccessRecord {
        observed_at_ms,
        generation,
    })
}

fn read_locked(file: &mut File) -> io::Result<StorageAccessObservation> {
    match file.metadata()?.len() {
        0 => Ok(StorageAccessObservation::Unknown),
        n if n == BYTES as u64 => {
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = [0; BYTES];
            file.read_exact(&mut bytes)?;
            decode(&bytes).map(StorageAccessObservation::Recorded)
        }
        _ => Err(invalid()),
    }
}

/// Read one access record using a nonblocking shared file lock.
///
/// # Errors
///
/// Returns an error for contention, I/O, damaged metadata or unsupported versions. Corrupt records
/// are not interpreted as missing, repaired, or overwritten. The supplied file is not closed.
pub fn observe_access(file: &mut File) -> io::Result<StorageAccessObservation> {
    file.try_lock_shared().map_err(io::Error::from)?;
    let result = read_locked(file);
    file.unlock()?;
    result
}

/// Persist a successful meaningful read, or conservatively initialize an empty tracking file.
///
/// Takes a nonblocking exclusive lock and merges against the existing maximum timestamp, making
/// stale concurrent delivery and clock rollback non-decreasing. Identical/older observations do
/// not write. `kind` explicitly distinguishes meaningful consumption from maintenance/index scans;
/// those scans must not call this API. Each changed record is synced before success is returned.
///
/// An interrupted write may leave damaged metadata. Such damage disables eligibility and requires
/// explicit maintenance; this function never falls back to a possibly stale previous timestamp.
/// Callers must treat write errors as unknown access age, not continue tiering using cached state.
///
/// # Errors
///
/// Returns an error for contention, I/O, generation overflow, damaged metadata or future versions.
/// Callers must supply a confined regular file opened for reading and writing, without append mode,
/// and must not share its seek cursor concurrently. No file is created or replaced here.
pub fn record_access(
    file: &mut File,
    _kind: StorageAccessKind,
    now_ms: u64,
) -> io::Result<StorageAccessRecord> {
    file.try_lock().map_err(io::Error::from)?;
    let result = update_locked(file, now_ms);
    file.unlock()?;
    result
}

fn update_locked(file: &mut File, now_ms: u64) -> io::Result<StorageAccessRecord> {
    let next = match read_locked(file)? {
        StorageAccessObservation::Unknown => StorageAccessRecord {
            observed_at_ms: now_ms,
            generation: 1,
        },
        StorageAccessObservation::Recorded(previous) => {
            if now_ms <= previous.observed_at_ms {
                return Ok(previous);
            }
            StorageAccessRecord {
                observed_at_ms: now_ms,
                generation: previous.generation.checked_add(1).ok_or_else(invalid)?,
            }
        }
    };
    let mut bytes = [0; BYTES];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..16].copy_from_slice(&VERSION.to_le_bytes());
    bytes[16..24].copy_from_slice(&next.observed_at_ms.to_le_bytes());
    bytes[24..32].copy_from_slice(&next.generation.to_le_bytes());
    let checksum = Sha256::digest(&bytes[..32]);
    bytes[32..].copy_from_slice(&checksum);
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    file.sync_data()?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn session_tracking_requires_canonical_storage_and_rejects_links() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        assert!(record_session_access(root.path(), id, StorageAccessKind::History, 1).is_err());
        let session = root.path().join(id.to_string());
        assert!(!session.exists());
        fs::create_dir(&session).expect("session");
        fs::write(session.join("session.db"), b"canonical bytes").expect("canonical");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let tracking = session.join("storage-access.bin");
        symlink(outside.path(), &tracking).expect("symlink");
        assert!(record_session_access(root.path(), id, StorageAccessKind::History, 1).is_err());
        assert_eq!(outside.as_file().metadata().expect("metadata").len(), 0);
        fs::remove_file(&tracking).expect("remove test link");
        fs::hard_link(outside.path(), &tracking).expect("hard link");
        assert!(record_session_access(root.path(), id, StorageAccessKind::History, 1).is_err());
        fs::remove_file(&tracking).expect("remove test link");
        let record = record_session_access(root.path(), id, StorageAccessKind::History, 500)
            .expect("record");
        assert_eq!(record.observed_at_ms, ACCESS_WINDOW_MS);
        assert_eq!(
            fs::read(session.join("session.db")).expect("unchanged"),
            b"canonical bytes"
        );
        let mut file = File::open(tracking).expect("open");
        assert_eq!(
            observe_access(&mut file).expect("observe"),
            StorageAccessObservation::Recorded(record)
        );
    }

    #[test]
    fn coalescing_never_understates_access_or_overflows() {
        for now in [0, 1, 59_999, 60_000, 60_001, u64::MAX - 1, u64::MAX] {
            let rounded = conservative_access_time(now);
            assert!(rounded >= now);
            assert!(rounded - now < ACCESS_WINDOW_MS);
        }
        assert_eq!(
            conservative_access_time(1),
            conservative_access_time(59_999)
        );
        assert_eq!(conservative_access_time(60_000), 60_000);
        assert_eq!(conservative_access_time(60_001), 120_000);
    }

    #[cfg(unix)]
    #[test]
    fn session_reads_in_same_window_share_one_committed_update() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let session = root.path().join(id.to_string());
        std::fs::create_dir(&session).expect("session");
        std::fs::write(session.join("session.db"), b"canonical").expect("canonical");
        let first =
            record_session_access(root.path(), id, StorageAccessKind::History, 1).expect("first");
        for now in [2, 30_000, 59_999, 60_000] {
            assert_eq!(
                record_session_access(root.path(), id, StorageAccessKind::Artifact, now)
                    .expect("coalesced"),
                first
            );
        }
        let next = record_session_access(root.path(), id, StorageAccessKind::History, 60_001)
            .expect("next");
        assert_eq!(next.generation, first.generation + 1);
        assert_eq!(next.observed_at_ms, 120_000);
    }

    #[test]
    fn unknown_then_monotonic_and_duplicate_safe() {
        let mut file = tempfile::tempfile().expect("file");
        assert_eq!(
            observe_access(&mut file).expect("unknown"),
            StorageAccessObservation::Unknown
        );
        let first = record_access(&mut file, StorageAccessKind::History, 100).expect("first");
        assert_eq!(first.generation, 1);
        for time in [100, 99, 0] {
            assert_eq!(
                record_access(&mut file, StorageAccessKind::Artifact, time).expect("stale"),
                first
            );
        }
        let next = record_access(&mut file, StorageAccessKind::ModelContext, 200).expect("next");
        assert_eq!(
            next,
            StorageAccessRecord {
                observed_at_ms: 200,
                generation: 2
            }
        );
        assert_eq!(
            observe_access(&mut file).expect("read"),
            StorageAccessObservation::Recorded(next)
        );
        assert_eq!(file.metadata().expect("metadata").len(), BYTES as u64);
    }

    #[test]
    fn independently_opened_writers_merge_and_contention_is_bounded() {
        let named = tempfile::NamedTempFile::new().expect("file");
        let mut first = named.reopen().expect("first");
        let mut second = named.reopen().expect("second");
        first.try_lock().expect("exclusive");
        assert!(observe_access(&mut second).is_err());
        assert!(record_access(&mut second, StorageAccessKind::History, 100).is_err());
        first.unlock().expect("unlock");
        record_access(&mut first, StorageAccessKind::History, 200).expect("record");
        let result =
            record_access(&mut second, StorageAccessKind::Artifact, 100).expect("stale merge");
        assert_eq!(result.observed_at_ms, 200);
        assert_eq!(result.generation, 1);
        drop(first);
        drop(second);
        assert_eq!(
            observe_access(&mut named.reopen().expect("reopen")).expect("durable"),
            StorageAccessObservation::Recorded(result)
        );
    }

    #[test]
    fn damaged_records_are_preserved_and_never_reinitialized() {
        for length in [1, 31, 63, 64, 65, 1024] {
            let mut file = tempfile::tempfile().expect("file");
            let contents = vec![42; length];
            file.write_all(&contents).expect("damage");
            assert!(observe_access(&mut file).is_err());
            assert!(record_access(&mut file, StorageAccessKind::History, 500).is_err());
            file.rewind().expect("rewind");
            let mut actual = Vec::new();
            file.read_to_end(&mut actual).expect("read");
            assert_eq!(actual, contents);
        }
    }

    #[test]
    fn future_versions_and_generation_overflow_fail_closed() {
        for (version, generation) in [(2_u64, 1_u64), (VERSION, u64::MAX)] {
            let mut file = tempfile::tempfile().expect("file");
            record_access(&mut file, StorageAccessKind::Artifact, 1).expect("initialize");
            file.rewind().expect("rewind");
            let mut bytes = [0; BYTES];
            file.read_exact(&mut bytes).expect("read");
            bytes[8..16].copy_from_slice(&version.to_le_bytes());
            bytes[24..32].copy_from_slice(&generation.to_le_bytes());
            let checksum = Sha256::digest(&bytes[..32]);
            bytes[32..].copy_from_slice(&checksum);
            file.rewind().expect("rewind");
            file.write_all(&bytes).expect("write");
            assert!(record_access(&mut file, StorageAccessKind::History, 2).is_err());
            file.rewind().expect("rewind");
            let mut actual = [0; BYTES];
            file.read_exact(&mut actual).expect("read unchanged");
            assert_eq!(actual, bytes);
        }
    }
}
