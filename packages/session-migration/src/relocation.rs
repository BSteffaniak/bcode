//! Explicit cross-location session relocation.
//!
//! Relocation moves one session's canonical storage from the state location that owns it to
//! another configured location. It is an explicit maintenance operation, never something a
//! normal read, attach, or catalog path performs (`Repair is explicit`).
//!
//! The sequence is deliberately ordered so an interruption always leaves the **source**
//! authoritative:
//!
//! 1. Plan, without mutating anything.
//! 2. Copy every canonical byte to a staging directory in the destination, hashing as it goes.
//! 3. Verify the staged copy against the source digests.
//! 4. Publish the staged directory atomically into its canonical destination path.
//! 5. Only then unlink the source.
//!
//! A crash before step 4 leaves only a staging directory, which the next run discards and
//! rebuilds; the source is untouched. A crash during step 5 leaves both directories present,
//! which is reported as a conflict rather than silently merged
//! (`Canonical history is never silently merged`).
//!
//! Relocation is **crash-safe and idempotent, but not resumable**: an interrupted copy restarts
//! from zero rather than continuing, because treating partially copied canonical bytes as
//! authoritative would be unsafe. Abandoned staging is reclaimed by an explicit prune operation,
//! which distinguishes a live relocation from dead debris using an advisory lock rather than a
//! timing heuristic.
//!
//! This module owns discovery, classification, copy, verification, publish, and journalling.
//! Ownership coordination is injected by the caller, because this crate must not depend on the
//! session runtime's lease types — the same inversion `recover_accidental_epoch_session_root`
//! uses. That keeps relocation ownership-fenced without inverting the dependency direction
//! (`Dependencies point toward contracts`).

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Buffer size used for streaming relocation copies.
const RELOCATION_BUFFER_BYTES: usize = 128 * 1024;

/// Directory name prefix for in-progress relocation staging.
///
/// Staging lives inside the destination sessions root so the publish step is a same-filesystem
/// rename, which is what makes publication atomic. It is deliberately not a valid session ID,
/// so canonical session discovery ignores it.
const RELOCATION_STAGING_PREFIX: &str = "relocating-";

/// File name of the relocation journal inside a staging directory.
const RELOCATION_JOURNAL_FILE: &str = "relocation-journal.json";

/// File name of the advisory liveness lock inside a staging directory.
///
/// Held for the lifetime of an in-flight relocation. The operating system releases an advisory
/// file lock when the holding process dies, so a prune operation that *acquires* this lock knows
/// the previous owner is gone, while contention means a relocation is genuinely still running.
/// That is a real liveness signal rather than a timing heuristic.
const RELOCATION_LOCK_FILE: &str = "relocation.lock";

/// Session-owned artifact directory name, a sibling of canonical session directories.
const SESSION_ARTIFACTS_DIR: &str = "session-artifacts";

/// Why one session cannot be relocated right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRelocationBlock {
    /// A live owner still holds the session in the source location.
    BlockedByOwner,
    /// The destination already contains canonical storage for this session ID.
    ///
    /// Never resolved automatically: two canonical roots claiming one session ID is exactly the
    /// ambiguity that must be surfaced rather than merged.
    DestinationConflict,
    /// The source canonical storage is missing or unreadable.
    Unreadable,
}

/// One session's relocation classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRelocationEntry {
    /// Session under consideration.
    pub session_id: SessionId,
    /// Total canonical bytes to copy.
    pub canonical_bytes: u64,
    /// Whether this session carries artifacts pinned by absolute recorded URIs.
    ///
    /// Absolute `file://` URIs already recorded in canonical history cannot be rewritten
    /// (`Event history is canonical`), so relocation copies pinned artifact bytes to the
    /// destination and retains the source copy, leaving the recorded path resolvable.
    pub has_pinned_artifacts: bool,
    /// Blocking reason, when this session cannot be relocated.
    pub blocked: Option<SessionRelocationBlock>,
}

impl SessionRelocationEntry {
    /// Return whether this session can be relocated as planned.
    #[must_use]
    pub const fn is_relocatable(&self) -> bool {
        self.blocked.is_none()
    }
}

/// Non-mutating relocation plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRelocationPlan {
    /// Sessions that can be relocated.
    pub relocatable: Vec<SessionRelocationEntry>,
    /// Sessions that cannot be relocated, with their reason.
    pub blocked: Vec<SessionRelocationEntry>,
}

impl SessionRelocationPlan {
    /// Return whether the plan would relocate nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.relocatable.is_empty()
    }

    /// Return total canonical bytes the plan would copy.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.relocatable.iter().fold(0, |total, entry| {
            total.saturating_add(entry.canonical_bytes)
        })
    }

    /// Return sessions whose artifacts stay readable at the source after relocation.
    #[must_use]
    pub fn pinned_artifact_sessions(&self) -> Vec<SessionId> {
        self.relocatable
            .iter()
            .filter(|entry| entry.has_pinned_artifacts)
            .map(|entry| entry.session_id)
            .collect()
    }
}

/// Outcome of one applied relocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRelocationReport {
    /// Sessions whose canonical storage now lives in the destination.
    pub relocated: Vec<SessionId>,
    /// Sessions skipped, with their reason.
    pub blocked: Vec<SessionRelocationEntry>,
    /// Sessions whose pinned artifacts were copied and retained at the source.
    pub retained_pinned_artifacts: Vec<SessionId>,
}

/// Durable record of one in-flight relocation, used for diagnosis rather than resume.
///
/// The journal is written into the staging directory before any destination bytes exist, so an
/// operator inspecting abandoned staging can see which session it belonged to and how far it got.
///
/// It deliberately does **not** drive resume: an interrupted copy is discarded and re-copied rather
/// than continued, because trusting a partially copied canonical database would risk treating
/// unverified bytes as authoritative. Staging is derived state, so discarding it only costs a
/// re-copy, never canonical history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRelocationJournal {
    /// Journal schema version.
    pub schema_version: u32,
    /// Session being relocated.
    pub session_id: SessionId,
    /// Absolute source sessions root.
    pub source_root: PathBuf,
    /// Absolute destination sessions root.
    pub destination_root: PathBuf,
    /// Stage reached before the process last stopped.
    pub stage: SessionRelocationStage,
}

/// Current journal schema version.
pub const SESSION_RELOCATION_JOURNAL_SCHEMA_VERSION: u32 = 1;

/// Stages recorded in the journal before publication.
///
/// There is no `Published` stage: publication is a single atomic `rename`, after which the staging
/// directory and its journal no longer exist, so no journal can ever describe a published move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRelocationStage {
    /// Staging directory created; copy not finished.
    Copying,
    /// Copy finished and verified; publish not yet performed.
    Verified,
}

/// Relocation failures.
#[derive(Debug, Error)]
pub enum SessionRelocationError<E> {
    /// Filesystem discovery, copy, verification, publish, or cleanup failed.
    #[error("session relocation io failed at {}: {source}", path.display())]
    Io {
        /// Path being operated on.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// Copied bytes did not match the source.
    #[error(
        "session relocation verification failed for {session_id} at {}: staged copy does not match the source, so the source remains authoritative",
        relative_path.display()
    )]
    Verification {
        /// Session being relocated.
        session_id: SessionId,
        /// File that failed verification.
        relative_path: PathBuf,
    },
    /// Source and destination resolve to the same location.
    #[error("session relocation source and destination are the same location: {}", root.display())]
    SameLocation {
        /// The shared root.
        root: PathBuf,
    },
    /// Injected ownership coordination failed.
    #[error("session relocation coordination failed: {0}")]
    Coordination(E),
}

impl<E> SessionRelocationError<E> {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Ownership decision for one session, supplied by the session runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRelocationOwnership {
    /// No live owner; relocation may proceed.
    Available,
    /// A live owner holds the session; relocation must refuse.
    BlockedByOwner,
}

/// Build a non-mutating relocation plan.
///
/// Nothing is copied, published, or deleted. No canonical database is opened and no lease is
/// taken beyond the caller's own ownership probe, so this is safe to run against a live location
/// (`Normal session reads are bounded and non-mutating`).
///
/// `sessions` selects which session IDs to consider; an empty slice plans every session found in
/// the source root.
///
/// # Errors
///
/// Returns an error when discovery fails, the roots are identical, or the ownership probe fails.
pub fn plan_session_relocation<E>(
    source_root: &Path,
    destination_root: &Path,
    sessions: &[SessionId],
    mut ownership: impl FnMut(SessionId, &Path) -> Result<SessionRelocationOwnership, E>,
) -> Result<SessionRelocationPlan, SessionRelocationError<E>> {
    if same_root(source_root, destination_root) {
        return Err(SessionRelocationError::SameLocation {
            root: source_root.to_path_buf(),
        });
    }
    let candidates = if sessions.is_empty() {
        canonical_session_ids(source_root)?
    } else {
        let mut selected = sessions.to_vec();
        selected.sort_unstable();
        selected.dedup();
        selected
    };

    let mut plan = SessionRelocationPlan::default();
    for session_id in candidates {
        let source_dir = source_root.join(session_id.to_string());
        if !source_dir.join("session.db").is_file() {
            plan.blocked.push(SessionRelocationEntry {
                session_id,
                canonical_bytes: 0,
                has_pinned_artifacts: false,
                blocked: Some(SessionRelocationBlock::Unreadable),
            });
            continue;
        }
        if destination_root.join(session_id.to_string()).exists() {
            plan.blocked.push(SessionRelocationEntry {
                session_id,
                canonical_bytes: 0,
                has_pinned_artifacts: false,
                blocked: Some(SessionRelocationBlock::DestinationConflict),
            });
            continue;
        }
        let Ok(files) = relocation_files(&source_dir, &source_dir) else {
            plan.blocked.push(SessionRelocationEntry {
                session_id,
                canonical_bytes: 0,
                has_pinned_artifacts: false,
                blocked: Some(SessionRelocationBlock::Unreadable),
            });
            continue;
        };
        let canonical_bytes = files
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.bytes));
        let has_pinned_artifacts = session_artifact_dir(source_root, session_id).exists();
        let entry = SessionRelocationEntry {
            session_id,
            canonical_bytes,
            has_pinned_artifacts,
            blocked: None,
        };
        match ownership(session_id, source_root).map_err(SessionRelocationError::Coordination)? {
            SessionRelocationOwnership::BlockedByOwner => {
                plan.blocked.push(SessionRelocationEntry {
                    blocked: Some(SessionRelocationBlock::BlockedByOwner),
                    ..entry
                });
            }
            SessionRelocationOwnership::Available => plan.relocatable.push(entry),
        }
    }
    Ok(plan)
}

/// Apply a relocation: verified copy, atomic publish, then source unlink.
///
/// `fence` must acquire and hold exclusive maintenance ownership of the session for the duration
/// of its callback, refusing every live owner, and invoke `apply` while that ownership is held.
/// Relocation performs no mutation outside that callback, so a session cannot be moved out from
/// under a live owner (`Active workflow execution is ownership-fenced`).
///
/// `apply` reports its own relocation failure through the returned outcome rather than through
/// the caller's error type, so the fence closure only has to decide ownership.
///
/// Derived state is not carried across: catalogs, projections, and search indexes are rebuilt at
/// the destination rather than copied (`Derived state is disposable`). Leases and locks are
/// per-root coordination state and are never copied.
///
/// # Errors
///
/// Returns an error when discovery, copy, verification, publish, unlink, or fencing fails.
pub fn relocate_sessions<E, F>(
    source_root: &Path,
    destination_root: &Path,
    plan: &SessionRelocationPlan,
    mut fence: F,
) -> Result<SessionRelocationReport, SessionRelocationError<E>>
where
    F: FnMut(SessionId, &mut dyn FnMut()) -> Result<SessionRelocationOwnership, E>,
{
    if same_root(source_root, destination_root) {
        return Err(SessionRelocationError::SameLocation {
            root: source_root.to_path_buf(),
        });
    }
    let mut report = SessionRelocationReport {
        blocked: plan.blocked.clone(),
        ..SessionRelocationReport::default()
    };
    for entry in &plan.relocatable {
        let session_id = entry.session_id;
        let mut outcome: Option<Result<(), SessionRelocationError<E>>> = None;
        let mut relocate_once = || {
            outcome = Some(relocate_one_session(
                source_root,
                destination_root,
                session_id,
            ));
        };
        let ownership =
            fence(session_id, &mut relocate_once).map_err(SessionRelocationError::Coordination)?;
        if ownership == SessionRelocationOwnership::BlockedByOwner {
            report.blocked.push(SessionRelocationEntry {
                blocked: Some(SessionRelocationBlock::BlockedByOwner),
                ..entry.clone()
            });
            continue;
        }
        // A fence that reported availability without invoking `apply` performed no work; treat it
        // as blocked rather than silently reporting a relocation that never happened.
        match outcome {
            Some(Ok(())) => {
                report.relocated.push(session_id);
                if entry.has_pinned_artifacts {
                    report.retained_pinned_artifacts.push(session_id);
                }
            }
            Some(Err(error)) => return Err(error),
            None => report.blocked.push(SessionRelocationEntry {
                blocked: Some(SessionRelocationBlock::BlockedByOwner),
                ..entry.clone()
            }),
        }
    }
    Ok(report)
}

/// Relocate exactly one session's canonical storage and its session-owned artifacts.
fn relocate_one_session<E>(
    source_root: &Path,
    destination_root: &Path,
    session_id: SessionId,
) -> Result<(), SessionRelocationError<E>> {
    let source_dir = source_root.join(session_id.to_string());
    let destination_dir = destination_root.join(session_id.to_string());

    // Re-check under the fence: the destination may have gained this session between planning
    // and application. Two canonical roots for one session ID are never merged.
    if destination_dir.exists() {
        return Ok(());
    }

    let staging = destination_root.join(format!("{RELOCATION_STAGING_PREFIX}{session_id}"));
    // Any staging directory left by an earlier interrupted run is derived state; discard it and
    // re-copy rather than trusting a partial copy of canonical history.
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .map_err(|source| SessionRelocationError::io(&staging, source))?;
    }
    fs::create_dir_all(&staging).map_err(|source| SessionRelocationError::io(&staging, source))?;

    // Hold an advisory lock for the whole move so a concurrent prune can tell this live
    // relocation from abandoned debris. The OS releases it if this process dies, which is what
    // makes prune's liveness check a fact rather than a timing guess.
    let lock_path = staging.join(RELOCATION_LOCK_FILE);
    let lock_file = File::create(&lock_path)
        .map_err(|source| SessionRelocationError::io(&lock_path, source))?;
    lock_file
        .lock()
        .map_err(|source| SessionRelocationError::io(&lock_path, source))?;

    write_journal(
        &staging,
        &SessionRelocationJournal {
            schema_version: SESSION_RELOCATION_JOURNAL_SCHEMA_VERSION,
            session_id,
            source_root: source_root.to_path_buf(),
            destination_root: destination_root.to_path_buf(),
            stage: SessionRelocationStage::Copying,
        },
    )?;

    let files = relocation_files(&source_dir, &source_dir)
        .map_err(|source| SessionRelocationError::io(&source_dir, source))?;
    let digests = copy_and_hash(&source_dir, &staging, &files)?;
    verify_copy(session_id, &staging, &files, &digests)?;

    update_journal_stage(&staging, SessionRelocationStage::Verified)?;
    abort_at_relocation_crash_boundary("before_publish");

    // Publish atomically. The journal and lock both live inside the staging directory, so remove
    // them first: canonical storage must not gain relocation artifacts.
    let journal_path = staging.join(RELOCATION_JOURNAL_FILE);
    fs::remove_file(&journal_path)
        .map_err(|source| SessionRelocationError::io(&journal_path, source))?;
    // Release before unlinking so no handle outlives the file it locks.
    drop(lock_file);
    fs::remove_file(&lock_path).map_err(|source| SessionRelocationError::io(&lock_path, source))?;
    fs::rename(&staging, &destination_dir)
        .map_err(|source| SessionRelocationError::io(&destination_dir, source))?;

    abort_at_relocation_crash_boundary("before_source_unlink");

    // Pinned artifacts are copied to the destination and the source copy is retained, so
    // absolute URIs already recorded in canonical history stay resolvable while the relocated
    // session resolves artifacts at its new root.
    copy_session_artifacts(source_root, destination_root, session_id)?;

    fs::remove_dir_all(&source_dir)
        .map_err(|source| SessionRelocationError::io(&source_dir, source))?;
    Ok(())
}

/// Copy session-owned artifacts to the destination, retaining the source copy.
fn copy_session_artifacts<E>(
    source_root: &Path,
    destination_root: &Path,
    session_id: SessionId,
) -> Result<(), SessionRelocationError<E>> {
    let source_artifacts = session_artifact_dir(source_root, session_id);
    if !source_artifacts.exists() {
        return Ok(());
    }
    let destination_artifacts = session_artifact_dir(destination_root, session_id);
    if destination_artifacts.exists() {
        return Ok(());
    }
    let files = relocation_files(&source_artifacts, &source_artifacts)
        .map_err(|source| SessionRelocationError::io(&source_artifacts, source))?;
    fs::create_dir_all(&destination_artifacts)
        .map_err(|source| SessionRelocationError::io(&destination_artifacts, source))?;
    let digests = copy_and_hash(&source_artifacts, &destination_artifacts, &files)?;
    verify_copy(session_id, &destination_artifacts, &files, &digests)?;
    Ok(())
}

/// Return the session-owned artifact directory for one session under a sessions root.
#[must_use]
pub fn session_artifact_dir(sessions_root: &Path, session_id: SessionId) -> PathBuf {
    sessions_root
        .join(SESSION_ARTIFACTS_DIR)
        .join(session_id.to_string())
}

#[derive(Debug, Clone)]
struct RelocationFile {
    relative_path: PathBuf,
    bytes: u64,
}

fn relocation_files(root: &Path, directory: &Path) -> std::io::Result<Vec<RelocationFile>> {
    let mut files = Vec::new();
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            files.extend(relocation_files(root, &path)?);
        } else if metadata.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .to_path_buf();
            files.push(RelocationFile {
                relative_path,
                bytes: metadata.len(),
            });
        }
    }
    Ok(files)
}

fn copy_and_hash<E>(
    source: &Path,
    destination: &Path,
    files: &[RelocationFile],
) -> Result<BTreeMap<PathBuf, [u8; 32]>, SessionRelocationError<E>> {
    let mut digests = BTreeMap::new();
    for file in files {
        let source_path = source.join(&file.relative_path);
        let destination_path = destination.join(&file.relative_path);
        let parent = destination_path.parent().ok_or_else(|| {
            SessionRelocationError::io(
                &destination_path,
                std::io::Error::other("relocation destination file has no parent"),
            )
        })?;
        fs::create_dir_all(parent).map_err(|source| SessionRelocationError::io(parent, source))?;
        let reader = File::open(&source_path)
            .map_err(|source| SessionRelocationError::io(&source_path, source))?;
        let mut reader = BufReader::with_capacity(RELOCATION_BUFFER_BYTES, reader);
        let destination_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination_path)
            .map_err(|source| SessionRelocationError::io(&destination_path, source))?;
        let mut writer = BufWriter::with_capacity(RELOCATION_BUFFER_BYTES, destination_file);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; RELOCATION_BUFFER_BYTES];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|source| SessionRelocationError::io(&source_path, source))?;
            if read == 0 {
                break;
            }
            writer
                .write_all(&buffer[..read])
                .map_err(|source| SessionRelocationError::io(&destination_path, source))?;
            abort_at_relocation_crash_boundary("during_copy");
            hasher.update(&buffer[..read]);
        }
        writer
            .flush()
            .map_err(|source| SessionRelocationError::io(&destination_path, source))?;
        let permissions = fs::metadata(&source_path)
            .map_err(|source| SessionRelocationError::io(&source_path, source))?
            .permissions();
        fs::set_permissions(&destination_path, permissions)
            .map_err(|source| SessionRelocationError::io(&destination_path, source))?;
        digests.insert(file.relative_path.clone(), hasher.finalize().into());
    }
    Ok(digests)
}

fn verify_copy<E>(
    session_id: SessionId,
    destination: &Path,
    files: &[RelocationFile],
    digests: &BTreeMap<PathBuf, [u8; 32]>,
) -> Result<(), SessionRelocationError<E>> {
    abort_at_relocation_crash_boundary("before_verification");
    for file in files {
        let destination_path = destination.join(&file.relative_path);
        let length = fs::metadata(&destination_path)
            .map_err(|source| SessionRelocationError::io(&destination_path, source))?
            .len();
        if length != file.bytes {
            return Err(SessionRelocationError::Verification {
                session_id,
                relative_path: file.relative_path.clone(),
            });
        }
        let actual = hash_file(&destination_path)?;
        if digests.get(&file.relative_path) != Some(&actual) {
            return Err(SessionRelocationError::Verification {
                session_id,
                relative_path: file.relative_path.clone(),
            });
        }
    }
    Ok(())
}

fn hash_file<E>(path: &Path) -> Result<[u8; 32], SessionRelocationError<E>> {
    let file = File::open(path).map_err(|source| SessionRelocationError::io(path, source))?;
    let mut reader = BufReader::with_capacity(RELOCATION_BUFFER_BYTES, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; RELOCATION_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| SessionRelocationError::io(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn write_journal<E>(
    staging: &Path,
    journal: &SessionRelocationJournal,
) -> Result<(), SessionRelocationError<E>> {
    let path = staging.join(RELOCATION_JOURNAL_FILE);
    let encoded = serde_json::to_vec_pretty(journal).map_err(|error| {
        SessionRelocationError::io(&path, std::io::Error::other(error.to_string()))
    })?;
    fs::write(&path, encoded).map_err(|source| SessionRelocationError::io(&path, source))
}

fn update_journal_stage<E>(
    staging: &Path,
    stage: SessionRelocationStage,
) -> Result<(), SessionRelocationError<E>> {
    let path = staging.join(RELOCATION_JOURNAL_FILE);
    let contents = fs::read(&path).map_err(|source| SessionRelocationError::io(&path, source))?;
    let mut journal: SessionRelocationJournal =
        serde_json::from_slice(&contents).map_err(|error| {
            SessionRelocationError::io(&path, std::io::Error::other(error.to_string()))
        })?;
    journal.stage = stage;
    write_journal(staging, &journal)
}

/// Read a staging directory's relocation journal, when present and decodable.
///
/// An unknown future schema version is surfaced as an error rather than guessed
/// (`Unknown future state is not guessed`).
fn read_relocation_journal(
    staging: &Path,
) -> Result<Option<SessionRelocationJournal>, std::io::Error> {
    let path = staging.join(RELOCATION_JOURNAL_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let contents = fs::read(&path)?;
    let journal: SessionRelocationJournal = serde_json::from_slice(&contents)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if journal.schema_version > SESSION_RELOCATION_JOURNAL_SCHEMA_VERSION {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "relocation journal schema version {} is newer than supported version {SESSION_RELOCATION_JOURNAL_SCHEMA_VERSION}",
                journal.schema_version
            ),
        ));
    }
    Ok(Some(journal))
}

/// One abandoned-or-live staging directory found in a destination root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelocationStagingEntry {
    /// Session the staging directory belongs to.
    pub session_id: SessionId,
    /// Stage the interrupted relocation had reached, when its journal is readable.
    pub stage: Option<String>,
    /// Bytes currently staged.
    pub staged_bytes: u64,
    /// Whether a live relocation still holds this staging directory's advisory lock.
    ///
    /// A live entry is never pruned: doing so would delete a running relocation's working
    /// directory out from under it.
    pub live: bool,
}

/// Inventory and optional cleanup outcome for staging directories.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelocationStagingReport {
    /// Abandoned staging directories that can be pruned.
    pub prunable: Vec<RelocationStagingEntry>,
    /// Staging directories skipped because a live relocation still owns them.
    pub live: Vec<RelocationStagingEntry>,
    /// Sessions whose staging directories were actually removed.
    pub pruned: Vec<SessionId>,
}

impl RelocationStagingReport {
    /// Return whether nothing needs attention.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.prunable.is_empty() && self.live.is_empty()
    }

    /// Return total bytes held by prunable staging directories.
    #[must_use]
    pub fn prunable_bytes(&self) -> u64 {
        self.prunable
            .iter()
            .fold(0, |total, entry| total.saturating_add(entry.staged_bytes))
    }
}

/// Inventory staging directories left behind by interrupted relocations, optionally pruning them.
///
/// Staging directories are derived state: their presence is never proof that canonical history
/// moved (`Derived state is disposable`). Canonical session directories are never touched, because
/// only names carrying the staging prefix are considered.
///
/// Liveness is decided by advisory lock, not elapsed time: a staging directory whose lock can be
/// acquired had its owner die, while a contended lock means a relocation is still running and is
/// reported as live rather than pruned. With `apply` false this mutates nothing, matching the
/// inventory-then-apply shape of other explicit maintenance operations (`Repair is explicit`).
///
/// # Errors
///
/// Returns an error when the destination root cannot be enumerated or a prune fails.
pub fn prune_relocation_staging(
    destination_root: &Path,
    apply: bool,
) -> Result<RelocationStagingReport, std::io::Error> {
    let mut report = RelocationStagingReport::default();
    if !destination_root.exists() {
        return Ok(report);
    }
    let mut entries = fs::read_dir(destination_root)?
        .flatten()
        .filter_map(|entry| {
            entry.file_type().ok().filter(std::fs::FileType::is_dir)?;
            let name = entry.file_name();
            let suffix = name.to_str()?.strip_prefix(RELOCATION_STAGING_PREFIX)?;
            let session_id = suffix.parse::<SessionId>().ok()?;
            Some((session_id, entry.path()))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(session_id, _)| *session_id);

    for (session_id, path) in entries {
        let stage = read_relocation_journal(&path)
            .ok()
            .flatten()
            .map(|journal| match journal.stage {
                SessionRelocationStage::Copying => "copying".to_owned(),
                SessionRelocationStage::Verified => "verified".to_owned(),
            });
        let staged_bytes = relocation_files(&path, &path).map_or(0, |files| {
            files
                .iter()
                .fold(0_u64, |total, file| total.saturating_add(file.bytes))
        });
        let live = staging_is_live(&path)?;
        let entry = RelocationStagingEntry {
            session_id,
            stage,
            staged_bytes,
            live,
        };
        if live {
            report.live.push(entry);
            continue;
        }
        report.prunable.push(entry);
        if apply {
            fs::remove_dir_all(&path)?;
            report.pruned.push(session_id);
        }
    }
    Ok(report)
}

/// Report whether a live relocation still holds a staging directory's advisory lock.
///
/// Acquiring the lock proves the previous owner is gone, because the operating system releases
/// advisory locks when a process dies. The probe lock is released immediately, so this never
/// prevents a later real relocation from claiming the directory.
fn staging_is_live(staging: &Path) -> Result<bool, std::io::Error> {
    let lock_path = staging.join(RELOCATION_LOCK_FILE);
    if !lock_path.is_file() {
        // Staging without a lock file predates the lock or was interrupted before locking; it has
        // no live owner to protect.
        return Ok(false);
    }
    let file = File::open(&lock_path)?;
    match file.try_lock() {
        Ok(()) => {
            drop(file);
            Ok(false)
        }
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn canonical_session_ids<E>(root: &Path) -> Result<Vec<SessionId>, SessionRelocationError<E>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(root).map_err(|source| SessionRelocationError::io(root, source))?;
    let mut ids = entries
        .flatten()
        .filter_map(|entry| {
            entry.file_type().ok().filter(std::fs::FileType::is_dir)?;
            entry.file_name().to_str()?.parse::<SessionId>().ok()
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    Ok(ids)
}

fn same_root(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn abort_at_relocation_crash_boundary(boundary: &str) {
    if std::env::var("BCODE_SESSION_RELOCATION_CRASH_PHASE").as_deref() == Ok(boundary) {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    /// Ownership probe that always reports availability.
    ///
    /// The `Result` is structurally required to match the injected ownership-probe signature,
    /// which real callers implement with a fallible lease inspection.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "must match the injected ownership-probe signature"
    )]
    fn available(
        _session_id: SessionId,
        _root: &Path,
    ) -> Result<SessionRelocationOwnership, Infallible> {
        Ok(SessionRelocationOwnership::Available)
    }

    fn seed_session(root: &Path, session_id: SessionId, bytes: &[u8]) {
        let dir = root.join(session_id.to_string());
        fs::create_dir_all(&dir).expect("session dir");
        fs::write(dir.join("session.db"), bytes).expect("session db");
    }

    fn fence_available<E>()
    -> impl FnMut(SessionId, &mut dyn FnMut()) -> Result<SessionRelocationOwnership, E> {
        |_session_id, apply| {
            apply();
            Ok(SessionRelocationOwnership::Available)
        }
    }

    #[test]
    fn planning_is_non_mutating_and_reports_relocatable_sessions() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        seed_session(source.path(), session_id, b"canonical-events");

        let plan = plan_session_relocation(source.path(), destination.path(), &[], available)
            .expect("plan");

        assert_eq!(plan.relocatable.len(), 1);
        assert_eq!(plan.relocatable[0].session_id, session_id);
        assert_eq!(plan.relocatable[0].canonical_bytes, 16);
        assert!(plan.blocked.is_empty());
        assert!(
            source.path().join(session_id.to_string()).exists(),
            "planning must not move the source"
        );
        assert!(
            !destination.path().join(session_id.to_string()).exists(),
            "planning must not create the destination"
        );
    }

    #[test]
    fn planning_classifies_owners_conflicts_and_unreadable_sessions() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let owned = SessionId::new();
        let conflict = SessionId::new();
        let unreadable = SessionId::new();
        seed_session(source.path(), owned, b"a");
        seed_session(source.path(), conflict, b"b");
        // A canonical directory with no session.db cannot be relocated.
        fs::create_dir_all(source.path().join(unreadable.to_string())).expect("unreadable");
        seed_session(destination.path(), conflict, b"existing");

        let plan = plan_session_relocation(
            source.path(),
            destination.path(),
            &[],
            |session_id, _root| {
                if session_id == owned {
                    Ok::<_, Infallible>(SessionRelocationOwnership::BlockedByOwner)
                } else {
                    Ok(SessionRelocationOwnership::Available)
                }
            },
        )
        .expect("plan");

        assert!(plan.relocatable.is_empty());
        let blocked = plan
            .blocked
            .iter()
            .map(|entry| (entry.session_id, entry.blocked.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            blocked.get(&owned),
            Some(&Some(SessionRelocationBlock::BlockedByOwner))
        );
        assert_eq!(
            blocked.get(&conflict),
            Some(&Some(SessionRelocationBlock::DestinationConflict))
        );
        assert_eq!(
            blocked.get(&unreadable),
            Some(&Some(SessionRelocationBlock::Unreadable))
        );
    }

    #[test]
    fn relocation_moves_canonical_bytes_and_removes_the_source() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        seed_session(source.path(), session_id, b"canonical-events");

        let plan = plan_session_relocation(source.path(), destination.path(), &[], available)
            .expect("plan");
        let report = relocate_sessions::<Infallible, _>(
            source.path(),
            destination.path(),
            &plan,
            fence_available(),
        )
        .expect("relocate");

        assert_eq!(report.relocated, vec![session_id]);
        assert_eq!(
            fs::read(
                destination
                    .path()
                    .join(session_id.to_string())
                    .join("session.db")
            )
            .expect("destination bytes"),
            b"canonical-events",
            "canonical bytes must survive relocation unchanged"
        );
        assert!(
            !source.path().join(session_id.to_string()).exists(),
            "a completed relocation is a move, not a copy"
        );
    }

    #[test]
    fn relocation_never_leaves_a_staging_directory_or_journal_behind() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        seed_session(source.path(), session_id, b"canonical");

        let plan = plan_session_relocation(source.path(), destination.path(), &[], available)
            .expect("plan");
        relocate_sessions::<Infallible, _>(
            source.path(),
            destination.path(),
            &plan,
            fence_available(),
        )
        .expect("relocate");

        assert!(
            !destination
                .path()
                .join(format!("{RELOCATION_STAGING_PREFIX}{session_id}"))
                .exists(),
            "staging must be published, not retained"
        );
        assert!(
            !destination
                .path()
                .join(session_id.to_string())
                .join(RELOCATION_JOURNAL_FILE)
                .exists(),
            "canonical storage must not carry a relocation journal"
        );
        assert!(
            !destination
                .path()
                .join(session_id.to_string())
                .join(RELOCATION_LOCK_FILE)
                .exists(),
            "canonical storage must not carry a relocation lock file"
        );
    }

    #[test]
    fn a_live_owner_blocks_relocation_and_leaves_the_source_authoritative() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        seed_session(source.path(), session_id, b"canonical");

        // Plan while unowned, then apply under a fence that refuses: the fence is the
        // authority, so nothing may move even though the plan said relocatable.
        let plan = plan_session_relocation(source.path(), destination.path(), &[], available)
            .expect("plan");
        let report = relocate_sessions::<Infallible, _>(
            source.path(),
            destination.path(),
            &plan,
            |_session_id, _apply| Ok(SessionRelocationOwnership::BlockedByOwner),
        )
        .expect("relocate");

        assert!(report.relocated.is_empty());
        assert_eq!(
            report
                .blocked
                .first()
                .and_then(|entry| entry.blocked.clone()),
            Some(SessionRelocationBlock::BlockedByOwner)
        );
        assert!(
            source.path().join(session_id.to_string()).exists(),
            "a refused relocation must leave the source authoritative"
        );
        assert!(!destination.path().join(session_id.to_string()).exists());
    }

    #[test]
    fn a_destination_conflict_is_never_merged_or_overwritten() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        seed_session(source.path(), session_id, b"source-history");
        seed_session(destination.path(), session_id, b"destination-history");

        let plan = plan_session_relocation(source.path(), destination.path(), &[], available)
            .expect("plan");
        assert!(plan.relocatable.is_empty(), "a conflict is not relocatable");

        // Even if a stale plan claims it is relocatable, application re-checks under the fence.
        let forced = SessionRelocationPlan {
            relocatable: vec![SessionRelocationEntry {
                session_id,
                canonical_bytes: 0,
                has_pinned_artifacts: false,
                blocked: None,
            }],
            blocked: Vec::new(),
        };
        relocate_sessions::<Infallible, _>(
            source.path(),
            destination.path(),
            &forced,
            fence_available(),
        )
        .expect("relocate");

        assert_eq!(
            fs::read(
                destination
                    .path()
                    .join(session_id.to_string())
                    .join("session.db")
            )
            .expect("destination bytes"),
            b"destination-history",
            "an existing canonical destination must never be overwritten"
        );
        assert!(
            source.path().join(session_id.to_string()).exists(),
            "the source must survive a refused conflict"
        );
    }

    #[test]
    fn pinned_artifacts_are_copied_to_the_destination_and_retained_at_the_source() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        seed_session(source.path(), session_id, b"canonical");
        let artifacts = session_artifact_dir(source.path(), session_id);
        fs::create_dir_all(&artifacts).expect("artifacts");
        fs::write(artifacts.join("run.bcsr"), b"recording-bytes").expect("artifact");

        let plan = plan_session_relocation(source.path(), destination.path(), &[], available)
            .expect("plan");
        assert!(plan.relocatable[0].has_pinned_artifacts);
        assert_eq!(plan.pinned_artifact_sessions(), vec![session_id]);

        let report = relocate_sessions::<Infallible, _>(
            source.path(),
            destination.path(),
            &plan,
            fence_available(),
        )
        .expect("relocate");

        assert_eq!(report.retained_pinned_artifacts, vec![session_id]);
        assert_eq!(
            fs::read(session_artifact_dir(destination.path(), session_id).join("run.bcsr"))
                .expect("destination artifact"),
            b"recording-bytes",
            "the relocated session must resolve artifacts at its new root"
        );
        assert_eq!(
            fs::read(artifacts.join("run.bcsr")).expect("source artifact"),
            b"recording-bytes",
            "absolute URIs already in canonical history must stay resolvable at the source"
        );
    }

    #[test]
    fn relocation_refuses_identical_source_and_destination_roots() {
        let root = tempfile::tempdir().expect("root");
        let error = plan_session_relocation(root.path(), root.path(), &[], available)
            .expect_err("identical roots must be refused");
        assert!(matches!(error, SessionRelocationError::SameLocation { .. }));
    }

    #[test]
    fn abandoned_staging_is_inventoried_then_pruned_without_touching_canonical_sessions() {
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        let staging = destination
            .path()
            .join(format!("{RELOCATION_STAGING_PREFIX}{session_id}"));
        fs::create_dir_all(&staging).expect("staging");
        fs::write(staging.join("session.db"), b"partial").expect("partial copy");
        let canonical = SessionId::new();
        seed_session(destination.path(), canonical, b"untouched");

        // Inventory first: reporting must not mutate anything.
        let inventory = prune_relocation_staging(destination.path(), false).expect("inventory");
        assert_eq!(inventory.prunable.len(), 1);
        assert_eq!(inventory.prunable[0].session_id, session_id);
        assert_eq!(inventory.prunable[0].staged_bytes, 7);
        assert!(inventory.pruned.is_empty(), "inventory must not delete");
        assert!(staging.exists(), "inventory must not mutate staging");

        let report = prune_relocation_staging(destination.path(), true).expect("prune");

        assert_eq!(report.pruned, vec![session_id]);
        assert!(!staging.exists(), "abandoned staging must be pruned");
        assert!(
            destination.path().join(canonical.to_string()).exists(),
            "canonical sessions must never be pruned as staging"
        );
    }

    /// A staging directory whose advisory lock is still held belongs to a running relocation and
    /// must never be pruned, even with `apply`.
    #[test]
    fn live_staging_is_reported_rather_than_pruned() {
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        let staging = destination
            .path()
            .join(format!("{RELOCATION_STAGING_PREFIX}{session_id}"));
        fs::create_dir_all(&staging).expect("staging");
        let lock_path = staging.join(RELOCATION_LOCK_FILE);
        let held = File::create(&lock_path).expect("lock file");
        held.lock().expect("hold the lock like a live relocation");

        let report = prune_relocation_staging(destination.path(), true).expect("prune");

        assert!(
            report.prunable.is_empty(),
            "a live relocation is not prunable"
        );
        assert_eq!(report.live.len(), 1);
        assert_eq!(report.live[0].session_id, session_id);
        assert!(report.live[0].live);
        assert!(report.pruned.is_empty());
        assert!(
            staging.exists(),
            "pruning must never delete a running relocation's staging directory"
        );

        // Once the owner releases the lock, the same directory becomes prunable.
        drop(held);
        let after = prune_relocation_staging(destination.path(), true).expect("prune");
        assert_eq!(after.pruned, vec![session_id]);
        assert!(!staging.exists());
    }

    #[test]
    fn a_staging_journal_records_interrupted_progress() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        let staging = destination
            .path()
            .join(format!("{RELOCATION_STAGING_PREFIX}{session_id}"));
        fs::create_dir_all(&staging).expect("staging");
        write_journal::<Infallible>(
            &staging,
            &SessionRelocationJournal {
                schema_version: SESSION_RELOCATION_JOURNAL_SCHEMA_VERSION,
                session_id,
                source_root: source.path().to_path_buf(),
                destination_root: destination.path().to_path_buf(),
                stage: SessionRelocationStage::Copying,
            },
        )
        .expect("journal");

        let journal = read_relocation_journal(&staging)
            .expect("read")
            .expect("journal present");
        assert_eq!(journal.session_id, session_id);
        assert_eq!(journal.stage, SessionRelocationStage::Copying);

        update_journal_stage::<Infallible>(&staging, SessionRelocationStage::Verified)
            .expect("update");
        assert_eq!(
            read_relocation_journal(&staging)
                .expect("read")
                .expect("journal present")
                .stage,
            SessionRelocationStage::Verified
        );
    }

    #[test]
    fn an_unknown_future_journal_version_is_surfaced_rather_than_guessed() {
        let staging = tempfile::tempdir().expect("staging");
        let session_id = SessionId::new();
        let journal = serde_json::json!({
            "schema_version": SESSION_RELOCATION_JOURNAL_SCHEMA_VERSION + 1,
            "session_id": session_id.to_string(),
            "source_root": "/tmp/source",
            "destination_root": "/tmp/destination",
            "stage": "copying",
        });
        fs::write(
            staging.path().join(RELOCATION_JOURNAL_FILE),
            serde_json::to_vec(&journal).expect("encode"),
        )
        .expect("write");

        let error = read_relocation_journal(staging.path())
            .expect_err("a newer journal schema must not be interpreted as a known older form");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn derived_state_and_coordination_state_are_not_carried_across() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let session_id = SessionId::new();
        seed_session(source.path(), session_id, b"canonical");
        // Per-root coordination and derived state live outside the session directory.
        fs::create_dir_all(source.path().join("leases").join(session_id.to_string()))
            .expect("leases");
        fs::create_dir_all(source.path().join("locks")).expect("locks");
        fs::write(source.path().join("catalog.db"), b"derived-catalog").expect("catalog");

        let plan = plan_session_relocation(source.path(), destination.path(), &[], available)
            .expect("plan");
        relocate_sessions::<Infallible, _>(
            source.path(),
            destination.path(),
            &plan,
            fence_available(),
        )
        .expect("relocate");

        assert!(
            !destination.path().join("leases").exists(),
            "leases are per-root coordination state and must not be copied"
        );
        assert!(
            !destination.path().join("locks").exists(),
            "locks are per-root coordination state and must not be copied"
        );
        assert!(
            !destination.path().join("catalog.db").exists(),
            "derived catalogs are rebuilt at the destination, not migrated"
        );
    }

    #[test]
    fn selecting_explicit_sessions_plans_only_those_sessions() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let wanted = SessionId::new();
        let other = SessionId::new();
        seed_session(source.path(), wanted, b"a");
        seed_session(source.path(), other, b"b");

        let plan = plan_session_relocation(source.path(), destination.path(), &[wanted], available)
            .expect("plan");

        assert_eq!(plan.relocatable.len(), 1);
        assert_eq!(plan.relocatable[0].session_id, wanted);
    }

    #[test]
    #[ignore = "subprocess helper for relocation_process_crash_boundaries_preserve_the_source"]
    fn relocation_crash_helper() {
        let source = PathBuf::from(
            std::env::var_os("BCODE_SESSION_RELOCATION_CRASH_SOURCE").expect("source"),
        );
        let destination = PathBuf::from(
            std::env::var_os("BCODE_SESSION_RELOCATION_CRASH_DESTINATION").expect("destination"),
        );
        let session_id = std::env::var("BCODE_SESSION_RELOCATION_CRASH_SESSION")
            .expect("session")
            .parse::<SessionId>()
            .expect("session id");
        let plan = SessionRelocationPlan {
            relocatable: vec![SessionRelocationEntry {
                session_id,
                canonical_bytes: 0,
                has_pinned_artifacts: false,
                blocked: None,
            }],
            blocked: Vec::new(),
        };
        relocate_sessions::<Infallible, _>(&source, &destination, &plan, fence_available())
            .expect("relocation crash phase must abort first");
    }

    fn run_relocation_crash_child(
        source: &Path,
        destination: &Path,
        session_id: SessionId,
        phase: &str,
    ) {
        let status =
            std::process::Command::new(std::env::current_exe().expect("current test binary"))
                .args([
                    "--exact",
                    "relocation::tests::relocation_crash_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("BCODE_SESSION_RELOCATION_CRASH_SOURCE", source)
                .env("BCODE_SESSION_RELOCATION_CRASH_DESTINATION", destination)
                .env(
                    "BCODE_SESSION_RELOCATION_CRASH_SESSION",
                    session_id.to_string(),
                )
                .env("BCODE_SESSION_RELOCATION_CRASH_PHASE", phase)
                .status()
                .expect("run relocation crash child");
        assert!(
            !status.success(),
            "relocation helper must terminate abnormally"
        );
    }

    /// An interruption at any boundary before the source is unlinked must leave the source
    /// authoritative, and must never expose a partially copied canonical destination.
    #[test]
    fn relocation_process_crash_boundaries_preserve_the_source() {
        for phase in [
            "during_copy",
            "before_verification",
            "before_publish",
            "before_source_unlink",
        ] {
            let temp = tempfile::tempdir().expect("temp dir");
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            fs::create_dir_all(&source).expect("source");
            fs::create_dir_all(&destination).expect("destination");
            let session_id = SessionId::new();
            let bytes = vec![0x5a; RELOCATION_BUFFER_BYTES * 2 + 7];
            let session_dir = source.join(session_id.to_string());
            fs::create_dir_all(&session_dir).expect("session dir");
            fs::write(session_dir.join("session.db"), &bytes).expect("source DB");

            run_relocation_crash_child(&source, &destination, session_id, phase);

            assert_eq!(
                fs::read(session_dir.join("session.db")).expect("source survives"),
                bytes,
                "phase {phase} must leave source canonical bytes intact"
            );
            let published = destination.join(session_id.to_string());
            match phase {
                "during_copy" | "before_verification" => {
                    assert!(
                        !published.exists(),
                        "phase {phase} must not expose a canonical destination"
                    );
                }
                "before_publish" => {
                    assert!(
                        !published.exists(),
                        "publish had not happened yet at {phase}"
                    );
                    // Only staging may exist, and staging is prunable derived state. The aborted
                    // child held the advisory lock, so this also proves the OS released it on
                    // death: a dead owner's staging must be reclaimable, not stuck forever.
                    let staging =
                        destination.join(format!("{RELOCATION_STAGING_PREFIX}{session_id}"));
                    assert!(staging.exists(), "verified staging should be present");
                    let report = prune_relocation_staging(&destination, true).expect("prune");
                    assert!(
                        report.live.is_empty(),
                        "a crashed relocation must not look live"
                    );
                    assert_eq!(report.pruned, vec![session_id]);
                    assert!(!staging.exists());
                }
                "before_source_unlink" => {
                    // Both exist: the move completed publication but not source removal. This is
                    // surfaced as a conflict on the next plan, never merged automatically.
                    assert!(published.exists(), "publish completed at {phase}");
                    let plan =
                        plan_session_relocation(&source, &destination, &[session_id], available)
                            .expect("replan");
                    assert_eq!(
                        plan.blocked.first().and_then(|entry| entry.blocked.clone()),
                        Some(SessionRelocationBlock::DestinationConflict),
                        "a duplicated canonical root must surface as a conflict"
                    );
                    assert!(plan.relocatable.is_empty());
                }
                other => panic!("unhandled phase {other}"),
            }
        }
    }
}
