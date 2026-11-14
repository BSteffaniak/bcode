//! A session's published feed: what its owning daemon committed, readable by any process and any
//! build (see `docs/daemon-dormancy.md`).
//!
//! A session's database cannot be read by another process while its owner has it open, so a
//! client of another build follows what the owner publishes instead:
//!
//! * `feed/<session>/marker.json`: the **activity marker**, rewritten atomically when work on the
//!   session starts or stops, or when its owner takes or gives up the session. It names the
//!   owner (build, protocol, storage epoch, instance), so a reader knows whether it can talk to
//!   that daemon or can only follow.
//! * `feed/<session>/events.jsonl`: one record per committed event, appended after the commit,
//!   each with a checksum so a reader never trusts a torn or damaged line. A gap record marks
//!   events the owner could not publish (it lagged or rotated the file).
//!
//! The feed is a published view, never the session's history: it may be rotated or lost, and it
//! never affects the database. Readers follow it with filesystem notifications, without timers.

use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionId,
};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Version of the feed's files. A reader refuses a newer one rather than guess.
pub const FEED_FORMAT_VERSION: u32 = 1;

const MARKER_FILE: &str = "marker.json";
const EVENTS_FILE: &str = "events.jsonl";
/// A feed larger than this starts over (with a gap record): it is a view, not history.
const ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// The directory holding `session_id`'s feed under a sessions root.
#[must_use]
pub fn feed_dir(sessions_root: &Path, session_id: SessionId) -> PathBuf {
    sessions_root.join("feed").join(session_id.to_string())
}

/// The daemon that owns a session, as its feed names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedOwner {
    /// Build fingerprint of the owning daemon.
    pub build_fingerprint: String,
    /// Exact artifact of the owning daemon, when known.
    #[serde(default)]
    pub artifact_id: Option<String>,
    /// IPC protocol version the owner serves.
    pub protocol_version: u32,
    /// Session storage writer epoch the owner writes.
    #[serde(default)]
    pub storage_writer_epoch: Option<u32>,
    /// Owning daemon instance.
    #[serde(default)]
    pub daemon_instance_id: Option<String>,
    /// Owning daemon process id.
    pub pid: u32,
}

/// Whether a session has work, and who owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityMarker {
    /// Feed format version.
    pub format: u32,
    /// The session.
    pub session_id: SessionId,
    /// Its owning daemon.
    pub owner: FeedOwner,
    /// Whether the owner holds the session now (false once it gave it up or stopped).
    pub owning: bool,
    /// Whether a model turn or runtime work is running.
    pub busy: bool,
    /// Increases with every change, so a reader can tell a new marker from a repeated one.
    pub generation: u64,
    /// Event schema version of the events the owner publishes.
    pub event_schema_version: u16,
    /// When it was written, in Unix milliseconds.
    pub updated_at_ms: u64,
}

impl ActivityMarker {
    /// Whether a reader of this build can decode the feed's events.
    #[must_use]
    pub const fn readable(&self) -> bool {
        self.format <= FEED_FORMAT_VERSION
            && self.event_schema_version <= CURRENT_SESSION_EVENT_SCHEMA_VERSION
    }
}

/// One line of `events.jsonl`.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sum: Option<String>,
    /// The event's JSON, kept as the exact bytes the checksum covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    gap: bool,
}

fn checksum(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Write `bytes` to `path` atomically (temporary file, then rename). Not synced: the feed is a
/// view, and readers need only never to see a half-written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

/// The work running in a session, from its committed events: open model turns and unfinished
/// runtime work.
#[derive(Debug, Default)]
struct Work {
    turns: BTreeSet<String>,
    runtime: BTreeSet<String>,
}

impl Work {
    fn observe(&mut self, kind: &SessionEventKind) {
        match kind {
            SessionEventKind::ModelTurnStarted { turn_id, .. } => {
                self.turns.insert(turn_id.clone());
            }
            SessionEventKind::ModelTurnFinished { turn_id, .. } => {
                self.turns.remove(turn_id);
            }
            SessionEventKind::RuntimeWorkStarted { work_id, .. } => {
                self.runtime.insert(work_id.0.clone());
            }
            SessionEventKind::RuntimeWorkFinished {
                work_id, status, ..
            } if status.is_terminal() => {
                self.runtime.remove(&work_id.0);
            }
            _ => {}
        }
    }

    fn busy(&self) -> bool {
        !self.turns.is_empty() || !self.runtime.is_empty()
    }
}

/// Publishes one session's feed. Only the session's owning daemon writes it.
#[derive(Debug)]
pub struct FeedWriter {
    dir: PathBuf,
    session_id: SessionId,
    owner: FeedOwner,
    events: File,
    work: Work,
    marker: Option<ActivityMarker>,
}

impl FeedWriter {
    /// Open (or create) `session_id`'s feed under `sessions_root` as its owner, and mark it
    /// owned. Earlier records stay: a reader skips what it has seen by sequence.
    ///
    /// # Errors
    /// Returns an error when the feed directory or files cannot be created or written.
    pub fn open(sessions_root: &Path, session_id: SessionId, owner: FeedOwner) -> io::Result<Self> {
        let dir = feed_dir(sessions_root, session_id);
        fs::create_dir_all(&dir)?;
        let events = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(EVENTS_FILE))?;
        let mut writer = Self {
            dir,
            session_id,
            owner,
            events,
            work: Work::default(),
            marker: None,
        };
        writer.write_marker(true)?;
        Ok(writer)
    }

    /// Publish one committed event, and the marker when the session's work started or stopped.
    ///
    /// # Errors
    /// Returns an error when the feed cannot be written; the session itself is unaffected.
    pub fn publish(&mut self, event: &SessionEvent) -> io::Result<()> {
        let json = serde_json::to_string(event).map_err(io::Error::other)?;
        let record = Record {
            v: FEED_FORMAT_VERSION,
            seq: Some(event.sequence),
            sum: Some(checksum(json.as_bytes())),
            event: Some(json),
            gap: false,
        };
        self.append(&record)?;
        let busy = self.work.busy();
        self.work.observe(&event.kind);
        if self.work.busy() != busy {
            self.write_marker(true)?;
        }
        Ok(())
    }

    /// Record that events were not published (the owner lagged behind its commits). Readers
    /// know their view is incomplete from here.
    ///
    /// # Errors
    /// Returns an error when the feed cannot be written.
    pub fn gap(&mut self) -> io::Result<()> {
        self.append(&Record {
            v: FEED_FORMAT_VERSION,
            seq: None,
            sum: None,
            event: None,
            gap: true,
        })
    }

    /// Give the session up: the marker says it is no longer owned, and not busy.
    ///
    /// # Errors
    /// Returns an error when the marker cannot be written.
    pub fn release(mut self) -> io::Result<()> {
        self.work = Work::default();
        self.write_marker(false)
    }

    /// The marker as last written.
    #[must_use]
    pub const fn marker(&self) -> Option<&ActivityMarker> {
        self.marker.as_ref()
    }

    fn append(&mut self, record: &Record) -> io::Result<()> {
        if self.events.metadata()?.len() > ROTATE_BYTES {
            self.rotate()?;
        }
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        // One write per record, so a concurrent reader sees whole records or a torn tail.
        self.events.write_all(&line)
    }

    /// Start the file over with a gap record, replacing it atomically.
    fn rotate(&mut self) -> io::Result<()> {
        let path = self.dir.join(EVENTS_FILE);
        let mut line = serde_json::to_vec(&Record {
            v: FEED_FORMAT_VERSION,
            seq: None,
            sum: None,
            event: None,
            gap: true,
        })
        .map_err(io::Error::other)?;
        line.push(b'\n');
        write_atomic(&path, &line)?;
        self.events = OpenOptions::new().append(true).open(&path)?;
        Ok(())
    }

    fn write_marker(&mut self, owning: bool) -> io::Result<()> {
        let generation = self
            .marker
            .as_ref()
            .map_or(0, |marker| marker.generation.saturating_add(1));
        let marker = ActivityMarker {
            format: FEED_FORMAT_VERSION,
            session_id: self.session_id,
            owner: self.owner.clone(),
            owning,
            busy: owning && self.work.busy(),
            generation,
            event_schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            updated_at_ms: now_ms(),
        };
        let bytes = serde_json::to_vec_pretty(&marker).map_err(io::Error::other)?;
        write_atomic(&self.dir.join(MARKER_FILE), &bytes)?;
        self.marker = Some(marker);
        Ok(())
    }
}

/// Why a feed cannot be shown by this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedUnreadable {
    /// A newer Bcode build owns the session; its feed or events are a format this build does
    /// not know.
    NewerBuild,
    /// The marker is damaged.
    Damaged,
}

/// Read `session_id`'s activity marker. `Ok(None)` when no feed was ever published.
///
/// # Errors
/// Returns [`FeedUnreadable`] when the marker is damaged or from a newer format.
pub fn read_marker(
    sessions_root: &Path,
    session_id: SessionId,
) -> Result<Option<ActivityMarker>, FeedUnreadable> {
    let Ok(bytes) = fs::read(feed_dir(sessions_root, session_id).join(MARKER_FILE)) else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| FeedUnreadable::Damaged)?;
    let format = value
        .get("format")
        .and_then(serde_json::Value::as_u64)
        .ok_or(FeedUnreadable::Damaged)?;
    if format > u64::from(FEED_FORMAT_VERSION) {
        return Err(FeedUnreadable::NewerBuild);
    }
    let marker: ActivityMarker =
        serde_json::from_value(value).map_err(|_| FeedUnreadable::Damaged)?;
    if marker.readable() {
        Ok(Some(marker))
    } else {
        Err(FeedUnreadable::NewerBuild)
    }
}

/// What following a feed delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedUpdate {
    /// A committed event, in order.
    Event(Box<SessionEvent>),
    /// Events are missing before the next one; the view is incomplete.
    Gap,
    /// The marker changed (owner, owning, busy).
    Marker(Box<ActivityMarker>),
    /// The feed can no longer be shown by this build.
    Unreadable(FeedUnreadable),
}

/// Follows one session's feed from a sequence on, woken by filesystem notifications.
pub struct FeedFollower {
    sessions_root: PathBuf,
    session_id: SessionId,
    dir: PathBuf,
    offset: u64,
    /// Bytes of an incomplete last line, kept until it completes.
    partial: Vec<u8>,
    after: Option<u64>,
    marker: Option<ActivityMarker>,
    file_identity: Option<FileIdentity>,
    pending: std::collections::VecDeque<FeedUpdate>,
    changes: tokio::sync::mpsc::UnboundedReceiver<()>,
    // Kept for its lifetime: dropping it stops the notifications.
    _watcher: Option<notify::RecommendedWatcher>,
}

impl std::fmt::Debug for FeedFollower {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FeedFollower")
            .field("session_id", &self.session_id)
            .field("offset", &self.offset)
            .field("after", &self.after)
            .finish_non_exhaustive()
    }
}

/// What distinguishes a replaced file from the one being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

fn identity(metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        FileIdentity {
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        FileIdentity {
            created: metadata.created().ok(),
        }
    }
}

impl FeedFollower {
    /// Follow `session_id`'s feed, delivering events after `after` (all of them for `None`).
    /// Without notifications (the directory cannot be watched), only what is there now is read.
    ///
    /// # Errors
    /// Returns an error when the feed directory cannot be created.
    pub fn open(
        sessions_root: &Path,
        session_id: SessionId,
        after: Option<u64>,
    ) -> io::Result<Self> {
        use notify::Watcher as _;
        let dir = feed_dir(sessions_root, session_id);
        // Watching needs the directory; creating it is harmless (a writer creates it anyway).
        fs::create_dir_all(&dir)?;
        let (sender, changes) = tokio::sync::mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                let _ = sender.send(());
            }
        })
        .ok()
        .and_then(|mut watcher| {
            watcher
                .watch(&dir, notify::RecursiveMode::NonRecursive)
                .ok()
                .map(|()| watcher)
        });
        Ok(Self {
            sessions_root: sessions_root.to_path_buf(),
            session_id,
            dir,
            offset: 0,
            partial: Vec::new(),
            after,
            marker: None,
            file_identity: None,
            pending: std::collections::VecDeque::new(),
            changes,
            _watcher: watcher,
        })
    }

    /// The next update, waiting for the owner to publish one. `None` when the feed can no
    /// longer be watched.
    pub async fn next(&mut self) -> Option<FeedUpdate> {
        loop {
            if let Some(update) = self.pending.pop_front() {
                return Some(update);
            }
            self.poll_files();
            if !self.pending.is_empty() {
                continue;
            }
            self.changes.recv().await?;
            // Coalesce a burst of notifications into one read.
            while self.changes.try_recv().is_ok() {}
        }
    }

    /// Read what changed in the marker and the events file into `pending`.
    fn poll_files(&mut self) {
        match read_marker(&self.sessions_root, self.session_id) {
            // A new owner starts its generations over, so the whole marker is compared.
            Ok(Some(marker)) if self.marker.as_ref() != Some(&marker) => {
                self.marker = Some(marker.clone());
                self.pending.push_back(FeedUpdate::Marker(Box::new(marker)));
            }
            Ok(_) => {}
            Err(unreadable) => {
                self.pending.push_back(FeedUpdate::Unreadable(unreadable));
                return;
            }
        }
        if let Err(error) = self.read_events() {
            // A feed that disappears or cannot be read is an incomplete view, not a failure.
            if error.kind() != io::ErrorKind::NotFound {
                self.pending.push_back(FeedUpdate::Gap);
            }
        }
    }

    fn read_events(&mut self) -> io::Result<()> {
        let mut file = File::open(self.dir.join(EVENTS_FILE))?;
        let metadata = file.metadata()?;
        let current = identity(&metadata);
        if self.file_identity.is_some_and(|known| known != current) || metadata.len() < self.offset
        {
            // Rotated: start over; sequences already seen are skipped.
            self.offset = 0;
            self.partial.clear();
        }
        self.file_identity = Some(current);
        file.seek(io::SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.partial.extend_from_slice(&bytes);
        let complete = self
            .partial
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let lines: Vec<u8> = self.partial.drain(..complete).collect();
        for line in lines.split(|byte| *byte == b'\n') {
            let update =
                std::str::from_utf8(line).map_or(Some(FeedUpdate::Gap), |line| self.decode(line));
            if let Some(update) = update {
                self.pending.push_back(update);
            }
        }
        Ok(())
    }

    fn decode(&mut self, line: &str) -> Option<FeedUpdate> {
        if line.trim().is_empty() {
            return None;
        }
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            return Some(FeedUpdate::Gap);
        };
        if record.v > FEED_FORMAT_VERSION {
            return Some(FeedUpdate::Unreadable(FeedUnreadable::NewerBuild));
        }
        if record.gap {
            return Some(FeedUpdate::Gap);
        }
        let (Some(seq), Some(sum), Some(json)) = (record.seq, record.sum, record.event) else {
            return Some(FeedUpdate::Gap);
        };
        if self.after.is_some_and(|after| seq <= after) {
            return None;
        }
        if checksum(json.as_bytes()) != sum {
            return Some(FeedUpdate::Gap);
        }
        match serde_json::from_str::<SessionEvent>(&json) {
            Ok(event) => {
                self.after = Some(seq);
                Some(FeedUpdate::Event(Box::new(event)))
            }
            Err(_) => {
                let newer = serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .and_then(|value| value.get("schema_version")?.as_u64())
                    .is_some_and(|schema| schema > u64::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION));
                Some(if newer {
                    FeedUpdate::Unreadable(FeedUnreadable::NewerBuild)
                } else {
                    FeedUpdate::Gap
                })
            }
        }
    }
}

#[cfg(test)]
mod tests;
