//! Derived session index files and validation.

use crate::index::{self, EventFileFingerprint};
use crate::{SessionStoreError, projection};
use bcode_session_models::{SessionEvent, SessionId};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};

/// Current durable transcript projection index format version.
pub const TRANSCRIPT_INDEX_VERSION: u16 = 1;

/// Durable transcript projection index for one session.
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
        let spans = projection::build_transcript_projection(events, None)
            .iter()
            .map(index::TranscriptProjectionIndexEntry::from_item)
            .collect::<Vec<_>>();
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
    ) -> Result<(), TranscriptIndexInvalid> {
        if self.index_version != TRANSCRIPT_INDEX_VERSION {
            return Err(TranscriptIndexInvalid::Version {
                found: self.index_version,
                current: TRANSCRIPT_INDEX_VERSION,
            });
        }
        if self.session_id != session_id {
            return Err(TranscriptIndexInvalid::SessionId {
                found: self.session_id,
                expected: session_id,
            });
        }
        if &self.file != file {
            return Err(TranscriptIndexInvalid::Fingerprint);
        }
        validate_transcript_spans(&self.spans)
    }
}

/// Reason a transcript index cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptIndexInvalid {
    /// Index version does not match current format.
    Version { found: u16, current: u16 },
    /// Index was built for a different session.
    SessionId {
        found: SessionId,
        expected: SessionId,
    },
    /// Event-file fingerprint changed.
    Fingerprint,
    /// Projection spans are not sorted chronologically.
    Unsorted { previous_end: u64, next_start: u64 },
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
) -> Result<(), TranscriptIndexInvalid> {
    let mut previous_end = None;
    for span in spans {
        let start = span.source_range.start_sequence;
        if let Some(previous_end) = previous_end
            && start < previous_end
        {
            return Err(TranscriptIndexInvalid::Unsorted {
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
