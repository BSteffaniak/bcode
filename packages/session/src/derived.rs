//! Derived session index files and validation.

use crate::index::{self, EventFileFingerprint};
use crate::{SessionStoreError, projection};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId, SessionInputHistoryEntry};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};

/// Current durable transcript projection index format version.
pub const TRANSCRIPT_INDEX_VERSION: u16 = 1;

/// Current durable input history index format version.
pub const INPUT_HISTORY_INDEX_VERSION: u16 = 1;

/// Durable input history index for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputHistoryIndex {
    /// Index format version.
    pub index_version: u16,
    /// Indexed session id.
    pub session_id: SessionId,
    /// Event-file fingerprint this index was built from.
    pub file: EventFileFingerprint,
    /// Number of decoded events observed while building.
    pub event_count: usize,
    /// User-submitted prompts in chronological order.
    pub entries: Vec<SessionInputHistoryEntry>,
}

impl InputHistoryIndex {
    /// Build an input-history index from canonical session events.
    #[must_use]
    pub fn from_events(
        session_id: SessionId,
        file: EventFileFingerprint,
        events: &[SessionEvent],
    ) -> Self {
        let mut entries: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if let SessionEventKind::UserMessage { text, .. } = &event.kind {
                    Some(SessionInputHistoryEntry {
                        sequence: event.sequence,
                        text: text.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();
        entries.sort_by_key(|entry| entry.sequence);
        Self {
            index_version: INPUT_HISTORY_INDEX_VERSION,
            session_id,
            file,
            event_count: events.len(),
            entries,
        }
    }

    /// Validate this index against the current session event file.
    pub fn validate(
        &self,
        session_id: SessionId,
        file: &EventFileFingerprint,
    ) -> Result<(), DerivedIndexInvalid> {
        validate_header(
            self.index_version,
            INPUT_HISTORY_INDEX_VERSION,
            self.session_id,
            session_id,
            &self.file,
            file,
        )?;
        let mut previous = None;
        for entry in &self.entries {
            if let Some(previous) = previous
                && entry.sequence <= previous
            {
                return Err(DerivedIndexInvalid::Unsorted {
                    previous_end: previous,
                    next_start: entry.sequence,
                });
            }
            previous = Some(entry.sequence);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptIndex {
    /// Index format version.
    pub index_version: u16,
    /// Indexed session id.
    pub session_id: SessionId,
    /// Event-file fingerprint this index was built from.
    pub file: EventFileFingerprint,
    /// Number of decoded events observed while building.
    pub event_count: usize,
    /// Projection spans, sorted by source range.
    pub spans: Vec<index::TranscriptProjectionIndexEntry>,
}

impl TranscriptIndex {
    /// Build a transcript index from canonical session events.
    #[must_use]
    pub fn from_events(
        session_id: SessionId,
        file: EventFileFingerprint,
        events: &[SessionEvent],
    ) -> Self {
        let mut spans = projection::build_transcript_projection(events, None)
            .iter()
            .map(index::TranscriptProjectionIndexEntry::from_item)
            .collect::<Vec<_>>();
        spans.sort_by_key(|span| span.source_range.start_sequence);
        Self {
            index_version: TRANSCRIPT_INDEX_VERSION,
            session_id,
            file,
            event_count: events.len(),
            spans,
        }
    }

    /// Validate this index against the current session event file.
    pub fn validate(
        &self,
        session_id: SessionId,
        file: &EventFileFingerprint,
    ) -> Result<(), DerivedIndexInvalid> {
        validate_header(
            self.index_version,
            TRANSCRIPT_INDEX_VERSION,
            self.session_id,
            session_id,
            &self.file,
            file,
        )?;
        validate_transcript_spans(&self.spans)
    }
}

/// Reason a derived index cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedIndexInvalid {
    /// Index version does not match current format.
    Version { found: u16, current: u16 },
    /// Index was built for a different session.
    SessionId {
        found: SessionId,
        expected: SessionId,
    },
    /// Event-file fingerprint changed.
    Fingerprint,
    /// Entries are not sorted chronologically.
    Unsorted { previous_end: u64, next_start: u64 },
}

fn validate_header(
    found_version: u16,
    current_version: u16,
    found_session_id: SessionId,
    expected_session_id: SessionId,
    found_file: &EventFileFingerprint,
    expected_file: &EventFileFingerprint,
) -> Result<(), DerivedIndexInvalid> {
    if found_version != current_version {
        return Err(DerivedIndexInvalid::Version {
            found: found_version,
            current: current_version,
        });
    }
    if found_session_id != expected_session_id {
        return Err(DerivedIndexInvalid::SessionId {
            found: found_session_id,
            expected: expected_session_id,
        });
    }
    if found_file != expected_file {
        return Err(DerivedIndexInvalid::Fingerprint);
    }
    Ok(())
}

/// Return the per-session derived index directory.
#[must_use]
pub fn session_index_dir(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("index").join(session_id.to_string())
}

/// Return the transcript projection index path for a session.
#[must_use]
pub fn transcript_index_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_index_dir(root, session_id).join("transcript.jsonl")
}

/// Return the input-history index path for a session.
#[must_use]
pub fn input_history_index_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_index_dir(root, session_id).join("input_history.jsonl")
}

/// Load a fresh input-history index, rebuilding when it is absent or stale.
pub fn ensure_input_history_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<InputHistoryIndex, SessionStoreError> {
    let file = index::fingerprint(event_path)?;
    match load_input_history_index(root, session_id, &file) {
        Ok(index) => Ok(index),
        Err(DerivedIndexLoadError::NotFound | DerivedIndexLoadError::Invalid) => {
            rebuild_input_history_index(root, session_id, event_path)
        }
        Err(DerivedIndexLoadError::Store(error)) => Err(error),
    }
}

/// Rebuild and persist an input-history index.
pub fn rebuild_input_history_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<InputHistoryIndex, SessionStoreError> {
    let report = crate::reader::read_events(event_path)?;
    let file = index::fingerprint(event_path)?;
    let input_index = InputHistoryIndex::from_events(session_id, file, &report.events);
    write_input_history_index(root, &input_index)?;
    Ok(input_index)
}

/// Load a fresh transcript index, rebuilding when it is absent or stale.
pub fn ensure_transcript_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<TranscriptIndex, SessionStoreError> {
    let file = index::fingerprint(event_path)?;
    match load_transcript_index(root, session_id, &file) {
        Ok(index) => Ok(index),
        Err(TranscriptIndexLoadError::NotFound | TranscriptIndexLoadError::Invalid) => {
            rebuild_transcript_index(root, session_id, event_path)
        }
        Err(TranscriptIndexLoadError::Store(error)) => Err(error),
    }
}

/// Rebuild and persist a transcript projection index.
pub fn rebuild_transcript_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<TranscriptIndex, SessionStoreError> {
    let report = crate::reader::read_events(event_path)?;
    let file = index::fingerprint(event_path)?;
    let index = TranscriptIndex::from_events(session_id, file, &report.events);
    write_transcript_index(root, &index)?;
    Ok(index)
}

/// Persist an input-history index atomically.
pub fn write_input_history_index(
    root: &Path,
    index: &InputHistoryIndex,
) -> Result<(), SessionStoreError> {
    let path = input_history_index_path(root, index.session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(SessionStoreError::Io)?;
    }
    let tmp_path = path.with_extension("jsonl.tmp");
    let mut file = File::create(&tmp_path).map_err(SessionStoreError::Io)?;
    serde_json::to_writer(&mut file, &InputHistoryIndexHeader::from(index))
        .map_err(SessionStoreError::Index)?;
    writeln!(file).map_err(SessionStoreError::Io)?;
    for entry in &index.entries {
        serde_json::to_writer(&mut file, entry).map_err(SessionStoreError::Index)?;
        writeln!(file).map_err(SessionStoreError::Io)?;
    }
    file.sync_all().map_err(SessionStoreError::Io)?;
    drop(file);
    fs::rename(&tmp_path, &path).map_err(SessionStoreError::Io)
}

/// Persist a transcript projection index atomically.
pub fn write_transcript_index(
    root: &Path,
    index: &TranscriptIndex,
) -> Result<(), SessionStoreError> {
    let path = transcript_index_path(root, index.session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("jsonl.tmp");
    let mut file = File::create(&tmp_path)?;
    serde_json::to_writer(&mut file, &TranscriptIndexHeader::from(index))
        .map_err(SessionStoreError::Index)?;
    file.write_all(b"\n")?;
    for span in &index.spans {
        serde_json::to_writer(&mut file, span).map_err(SessionStoreError::Index)?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    fs::rename(tmp_path, path)?;
    Ok(())
}

fn load_input_history_index(
    root: &Path,
    session_id: SessionId,
    file: &EventFileFingerprint,
) -> Result<InputHistoryIndex, DerivedIndexLoadError> {
    let path = input_history_index_path(root, session_id);
    let index_file = File::open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DerivedIndexLoadError::NotFound
        } else {
            DerivedIndexLoadError::Store(SessionStoreError::Io(error))
        }
    })?;
    let mut lines = BufReader::new(index_file).lines();
    let Some(header_line) = lines.next().transpose().map_err(SessionStoreError::Io)? else {
        return Err(DerivedIndexLoadError::NotFound);
    };
    let header = serde_json::from_str::<InputHistoryIndexHeader>(&header_line)
        .map_err(SessionStoreError::Index)?;
    let mut entries = Vec::new();
    for line in lines {
        let line = line.map_err(SessionStoreError::Io)?;
        if line.trim().is_empty() {
            continue;
        }
        entries.push(
            serde_json::from_str::<SessionInputHistoryEntry>(&line)
                .map_err(SessionStoreError::Index)?,
        );
    }
    let input_index = InputHistoryIndex {
        index_version: header.index_version,
        session_id: header.session_id,
        file: header.file,
        event_count: header.event_count,
        entries,
    };
    input_index
        .validate(session_id, file)
        .map_err(|_error| DerivedIndexLoadError::Invalid)?;
    Ok(input_index)
}

fn load_transcript_index(
    root: &Path,
    session_id: SessionId,
    file: &EventFileFingerprint,
) -> Result<TranscriptIndex, TranscriptIndexLoadError> {
    let path = transcript_index_path(root, session_id);
    let index_file = File::open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TranscriptIndexLoadError::NotFound
        } else {
            TranscriptIndexLoadError::Store(SessionStoreError::Io(error))
        }
    })?;
    let mut lines = BufReader::new(index_file).lines();
    let Some(header_line) = lines.next().transpose().map_err(SessionStoreError::Io)? else {
        return Err(TranscriptIndexLoadError::NotFound);
    };
    let header = serde_json::from_str::<TranscriptIndexHeader>(&header_line)
        .map_err(SessionStoreError::Index)?;
    let mut spans = Vec::new();
    for line in lines {
        let line = line.map_err(SessionStoreError::Io)?;
        if line.trim().is_empty() {
            continue;
        }
        spans.push(
            serde_json::from_str::<index::TranscriptProjectionIndexEntry>(&line)
                .map_err(SessionStoreError::Index)?,
        );
    }
    let transcript_index = TranscriptIndex {
        index_version: header.index_version,
        session_id: header.session_id,
        file: header.file,
        event_count: header.event_count,
        spans,
    };
    transcript_index
        .validate(session_id, file)
        .map_err(|_error| TranscriptIndexLoadError::Invalid)?;
    Ok(transcript_index)
}

fn validate_transcript_spans(
    spans: &[index::TranscriptProjectionIndexEntry],
) -> Result<(), DerivedIndexInvalid> {
    let mut previous_end = None;
    for span in spans {
        let start = span.source_range.start_sequence;
        if let Some(previous_end) = previous_end
            && start < previous_end
        {
            return Err(DerivedIndexInvalid::Unsorted {
                previous_end,
                next_start: start,
            });
        }
        previous_end = Some(span.source_range.end_sequence);
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct TranscriptIndexHeader {
    index_version: u16,
    session_id: SessionId,
    file: EventFileFingerprint,
    event_count: usize,
}

impl From<&TranscriptIndex> for TranscriptIndexHeader {
    fn from(value: &TranscriptIndex) -> Self {
        Self {
            index_version: value.index_version,
            session_id: value.session_id,
            file: value.file.clone(),
            event_count: value.event_count,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct InputHistoryIndexHeader {
    index_version: u16,
    session_id: SessionId,
    file: EventFileFingerprint,
    event_count: usize,
}

impl From<&InputHistoryIndex> for InputHistoryIndexHeader {
    fn from(value: &InputHistoryIndex) -> Self {
        Self {
            index_version: value.index_version,
            session_id: value.session_id,
            file: value.file.clone(),
            event_count: value.event_count,
        }
    }
}

#[derive(Debug)]
enum DerivedIndexLoadError {
    NotFound,
    Invalid,
    Store(SessionStoreError),
}

impl From<SessionStoreError> for DerivedIndexLoadError {
    fn from(value: SessionStoreError) -> Self {
        Self::Store(value)
    }
}

#[derive(Debug)]
enum TranscriptIndexLoadError {
    NotFound,
    Invalid,
    Store(SessionStoreError),
}

impl From<SessionStoreError> for TranscriptIndexLoadError {
    fn from(value: SessionStoreError) -> Self {
        Self::Store(value)
    }
}
