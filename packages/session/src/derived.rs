//! Derived session index files and validation.

use crate::SessionStoreError;
use crate::index::{self, EventFileFingerprint};
use bcode_session_models::{
    ProjectionSourceRange, SessionEvent, SessionEventKind, SessionId, SessionInputHistoryEntry,
    ToolInvocationStreamEvent, TranscriptProjectionItemKind,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};

/// Current durable transcript projection index format version.
pub const TRANSCRIPT_INDEX_VERSION: u16 = 1;

/// Current durable input history index format version.
pub const INPUT_HISTORY_INDEX_VERSION: u16 = 1;

/// Maximum derived-index tail bytes normal attach/read paths will catch up inline.
const MAX_INLINE_DERIVED_CATCH_UP_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum derived-index tail events normal attach/read paths will catch up inline.
const MAX_INLINE_DERIVED_CATCH_UP_EVENTS: usize = 4096;

/// Derived index owned by the session store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedIndexKind {
    /// Transcript projection spans for fast attach/projection-window reads.
    Transcript,
    /// User prompt input history for composer navigation.
    InputHistory,
}

impl DerivedIndexKind {
    /// Stable derived-index identifier used in manifests and doctor output.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::InputHistory => "input_history",
        }
    }

    /// Current format version for this derived index.
    #[must_use]
    pub const fn current_version(self) -> u16 {
        match self {
            Self::Transcript => TRANSCRIPT_INDEX_VERSION,
            Self::InputHistory => INPUT_HISTORY_INDEX_VERSION,
        }
    }
}

/// Health for one derived index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedIndexHealth {
    /// Derived-index kind.
    pub kind: DerivedIndexKind,
    /// Whether the index file exists and validates against the event log.
    pub fresh: bool,
    /// Whether the index was rebuilt during the health check.
    pub rebuilt: bool,
    /// Number of source events covered by the index when available.
    pub event_count: Option<usize>,
    /// Number of derived records when available.
    pub item_count: Option<usize>,
    /// Human-readable issue when stale, missing, or invalid.
    pub issue: Option<String>,
}

/// Health for the derived-index manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedManifestHealth {
    /// Whether the manifest exists and validates against the event log and registered indexes.
    pub fresh: bool,
    /// Whether the manifest was rebuilt during health checking.
    pub rebuilt: bool,
    /// Human-readable issue when stale, missing, or invalid.
    pub issue: Option<String>,
}

/// Health for all derived indexes of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedHealth {
    /// Manifest health.
    pub manifest: DerivedManifestHealth,
    /// Registered derived-index health entries.
    pub indexes: Vec<DerivedIndexHealth>,
}

/// Current derived-index manifest format version.
pub const DERIVED_INDEX_MANIFEST_VERSION: u16 = 2;

/// Canonical event-log position through which a derived index has projected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedSourceCheckpoint {
    /// Number of canonical events projected.
    pub event_count: usize,
    /// Byte offset immediately after the last projected event frame.
    pub end_offset: u64,
    /// Sequence number of the last projected event, if any.
    pub last_sequence: Option<u64>,
}

/// Transcript-specific state needed to resume projection without rescanning history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptProjectorState {
    assistant: Option<PendingStreamState>,
    reasoning: Option<PendingStreamState>,
    tools: BTreeMap<String, PendingToolState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PendingStreamState {
    kind: TranscriptProjectionItemKind,
    start_sequence: u64,
    end_sequence: u64,
    content_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PendingToolState {
    start_sequence: u64,
    end_sequence: u64,
    content_bytes: usize,
    saw_stream_output: bool,
}

/// Manifest describing derived indexes available for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedIndexManifest {
    /// Manifest format version.
    pub manifest_version: u16,
    /// Indexed session id.
    pub session_id: SessionId,
    /// Event-file fingerprint shared by all manifest entries.
    pub file: EventFileFingerprint,
    /// Known derived-index entries.
    pub indexes: Vec<DerivedIndexManifestEntry>,
}

/// One derived-index manifest entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedIndexManifestEntry {
    /// Stable derived-index identifier.
    pub id: String,
    /// Index format version.
    pub version: u16,
    /// Relative path under the per-session derived-index directory.
    pub path: String,
    /// Canonical source checkpoint this index has projected through.
    pub checkpoint: DerivedSourceCheckpoint,
    /// Transcript projector state for resumable transcript indexes.
    #[serde(default)]
    pub transcript_state: Option<TranscriptProjectorState>,
    /// Number of derived records in the index.
    pub item_count: usize,
}

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
    /// Byte offset immediately after the last projected event frame.
    pub end_offset: u64,
    /// Last projected event sequence, if any.
    pub last_sequence: Option<u64>,
    /// User-submitted prompts in chronological order.
    pub entries: Vec<SessionInputHistoryEntry>,
}

impl InputHistoryIndex {
    /// Build an input-history index from canonical session events.
    #[must_use]
    pub fn from_events(
        session_id: SessionId,
        file: &EventFileFingerprint,
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
            file: file.clone(),
            event_count: events.len(),
            end_offset: file.len,
            last_sequence: events.last().map(|event| event.sequence),
            entries,
        }
    }

    /// Validate this index against the current session event file.
    pub fn validate(
        &self,
        session_id: SessionId,
        _file: Option<&EventFileFingerprint>,
    ) -> Result<(), DerivedIndexInvalid> {
        validate_header(
            self.index_version,
            INPUT_HISTORY_INDEX_VERSION,
            self.session_id,
            session_id,
            None,
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
    /// Event-file fingerprint from the last full rebuild.
    pub file: EventFileFingerprint,
    /// Number of decoded events observed while building.
    pub event_count: usize,
    /// Byte offset immediately after the last projected event frame.
    pub end_offset: u64,
    /// Last projected event sequence, if any.
    pub last_sequence: Option<u64>,
    /// Resumable projection state.
    pub projector_state: TranscriptProjectorState,
    /// Projection spans, sorted by source range.
    pub spans: Vec<index::TranscriptProjectionIndexEntry>,
}

impl TranscriptIndex {
    /// Build a transcript index from canonical session events.
    #[must_use]
    pub fn from_events(
        session_id: SessionId,
        file: &EventFileFingerprint,
        events: &[SessionEvent],
    ) -> Self {
        let (mut spans, projector_state) = project_transcript_events(events, None);
        spans.sort_by_key(|span| span.source_range.start_sequence);
        Self {
            index_version: TRANSCRIPT_INDEX_VERSION,
            session_id,
            file: file.clone(),
            event_count: events.len(),
            end_offset: file.len,
            last_sequence: events.last().map(|event| event.sequence),
            projector_state,
            spans,
        }
    }

    /// Validate this index against the current session event file.
    pub fn validate(
        &self,
        session_id: SessionId,
        _file: Option<&EventFileFingerprint>,
    ) -> Result<(), DerivedIndexInvalid> {
        validate_header(
            self.index_version,
            TRANSCRIPT_INDEX_VERSION,
            self.session_id,
            session_id,
            None,
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
    /// Entries are not sorted chronologically.
    Unsorted { previous_end: u64, next_start: u64 },
    /// Manifest is missing a required derived index.
    MissingManifestEntry { id: String },
    /// Manifest contains an unknown derived index.
    UnknownManifestEntry { id: String },
    /// Manifest points at the wrong derived-index path.
    ManifestPath {
        id: String,
        found: String,
        expected: String,
    },
    /// Manifest entry version does not match the registered index version.
    ManifestEntryVersion {
        id: String,
        found: u16,
        current: u16,
    },
    /// Manifest entry counts do not match the derived sidecar.
    ManifestEntryCount { id: String },
}

fn validate_header(
    found_version: u16,
    current_version: u16,
    found_session_id: SessionId,
    expected_session_id: SessionId,
    _max_source_len: Option<(&EventFileFingerprint, u64)>,
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
    Ok(())
}

/// Return the per-session derived index directory.
#[must_use]
pub fn session_index_dir(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("index").join(session_id.to_string())
}

/// Return the derived-index manifest path for a session.
#[must_use]
pub fn manifest_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_index_dir(root, session_id).join("manifest.json")
}

#[must_use]
pub fn transcript_index_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_index_dir(root, session_id).join("transcript.jsonl")
}

/// Return the input-history index path for a session.
#[must_use]
pub fn input_history_index_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_index_dir(root, session_id).join("input_history.jsonl")
}

/// Repair all registered derived indexes for a session by scanning the canonical event log.
///
/// This is an explicit repair/migration path only. Normal open/read/attach paths must not call it.
pub fn repair_rebuild_all_from_event_log(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<DerivedIndexManifest, SessionStoreError> {
    let report = crate::reader::read_events(event_path)?;
    let file = index::fingerprint(event_path)?;
    let transcript_index = TranscriptIndex::from_events(session_id, &file, &report.events);
    let input_index = InputHistoryIndex::from_events(session_id, &file, &report.events);
    write_transcript_index(root, &transcript_index)?;
    write_input_history_index(root, &input_index)?;
    remove_legacy_derived_state(root, session_id)?;
    write_manifest(root, session_id, file, &transcript_index, &input_index)
}

/// Initialize empty derived indexes immediately after creating a session.
///
/// This avoids treating newly-created sessions as missing derived state without scanning the event log.
pub fn initialize_empty_after_session_created(
    root: &Path,
    index: &index::SessionIndex,
) -> Result<DerivedIndexManifest, SessionStoreError> {
    if index.event_count != 1 {
        return Err(SessionStoreError::InvalidSessionId(
            "empty derived initialization is only valid immediately after session creation"
                .to_owned(),
        ));
    }
    if load_manifest(root, index.session_id).is_ok() {
        let existing = load_manifest(root, index.session_id).map_err(|_error| {
            SessionStoreError::InvalidSessionId("derived manifest disappeared".to_owned())
        })?;
        return Ok(existing);
    }
    let transcript_index = TranscriptIndex {
        index_version: TRANSCRIPT_INDEX_VERSION,
        session_id: index.session_id,
        file: index.file.clone(),
        event_count: index.event_count,
        end_offset: index.last_good_offset,
        last_sequence: index.next_sequence.checked_sub(1),
        projector_state: TranscriptProjectorState::default(),
        spans: Vec::new(),
    };
    let input_index = InputHistoryIndex {
        index_version: INPUT_HISTORY_INDEX_VERSION,
        session_id: index.session_id,
        file: index.file.clone(),
        event_count: index.event_count,
        end_offset: index.last_good_offset,
        last_sequence: index.next_sequence.checked_sub(1),
        entries: Vec::new(),
    };
    write_transcript_index(root, &transcript_index)?;
    write_input_history_index(root, &input_index)?;
    remove_legacy_derived_state(root, index.session_id)?;
    write_manifest(
        root,
        index.session_id,
        index.file.clone(),
        &transcript_index,
        &input_index,
    )
}

pub fn health_all(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
    fix: bool,
    force: bool,
) -> Result<DerivedHealth, SessionStoreError> {
    if force && fix {
        let manifest = repair_rebuild_all_from_event_log(root, session_id, event_path)?;
        return Ok(health_from_manifest(&manifest, true));
    }
    let file = index::fingerprint(event_path)?;
    match validate_all(root, session_id, &file) {
        Ok(manifest) => Ok(health_from_manifest(&manifest, false)),
        Err(issue) if fix => {
            let manifest = repair_rebuild_all_from_event_log(root, session_id, event_path)?;
            let mut health = health_from_manifest(&manifest, true);
            health.manifest.issue = Some(issue);
            Ok(health)
        }
        Err(issue) => Ok(DerivedHealth {
            manifest: DerivedManifestHealth {
                fresh: false,
                rebuilt: false,
                issue: Some(issue),
            },
            indexes: index_health_without_manifest(root, session_id, &file),
        }),
    }
}

fn validate_all(
    root: &Path,
    session_id: SessionId,
    file: &EventFileFingerprint,
) -> Result<DerivedIndexManifest, String> {
    let manifest = load_manifest(root, session_id).map_err(|error| format!("{error:?}"))?;
    validate_manifest(root, &manifest, session_id).map_err(|error| format!("{error:?}"))?;
    if !manifest_is_current(&manifest, file.len) {
        return Err("derived manifest is stale".to_owned());
    }
    load_indexes_from_manifest(root, session_id, &manifest)
        .map_err(|error| format!("derived sidecars: {error}"))?;
    Ok(manifest)
}

fn index_health_without_manifest(
    root: &Path,
    session_id: SessionId,
    _file: &EventFileFingerprint,
) -> Vec<DerivedIndexHealth> {
    let transcript = match load_transcript_index_lenient(root, session_id) {
        Ok(index) => DerivedIndexHealth {
            kind: DerivedIndexKind::Transcript,
            fresh: true,
            rebuilt: false,
            event_count: Some(index.event_count),
            item_count: Some(index.spans.len()),
            issue: None,
        },
        Err(error) => DerivedIndexHealth {
            kind: DerivedIndexKind::Transcript,
            fresh: false,
            rebuilt: false,
            event_count: None,
            item_count: None,
            issue: Some(issue_for_load_error(&error)),
        },
    };
    let input_history = match load_input_history_index_lenient(root, session_id) {
        Ok(index) => DerivedIndexHealth {
            kind: DerivedIndexKind::InputHistory,
            fresh: true,
            rebuilt: false,
            event_count: Some(index.event_count),
            item_count: Some(index.entries.len()),
            issue: None,
        },
        Err(error) => DerivedIndexHealth {
            kind: DerivedIndexKind::InputHistory,
            fresh: false,
            rebuilt: false,
            event_count: None,
            item_count: None,
            issue: Some(issue_for_load_error(&error)),
        },
    };
    vec![transcript, input_history]
}

/// Incrementally advance all derived indexes for one newly-appended event.
///
/// This is a normal append-path update only. It refuses to rebuild or catch up stale/missing
/// derived state; explicit repair/reindex paths own full replay behavior.
pub fn append_event_to_indexes(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
    entry: &index::SessionIndexEntry,
    event: &SessionEvent,
) -> Result<DerivedIndexManifest, SessionStoreError> {
    let manifest = load_manifest(root, session_id).map_err(|_error| {
        SessionStoreError::InvalidSessionId(
            "derived manifest missing; explicit repair is required".to_owned(),
        )
    })?;
    validate_manifest(root, &manifest, session_id).map_err(|error| invalid_to_store(&error))?;
    if manifest.indexes.iter().any(|manifest_entry| {
        manifest_entry.checkpoint.end_offset != entry.offset
            || manifest_entry.checkpoint.event_count
                != usize::try_from(event.sequence).unwrap_or(usize::MAX)
            || manifest_entry.checkpoint.last_sequence != event.sequence.checked_sub(1)
    }) {
        return Err(SessionStoreError::InvalidSessionId(
            "derived manifest is not at the append checkpoint; explicit repair is required"
                .to_owned(),
        ));
    }

    let (mut transcript, mut input_history) =
        load_indexes_from_manifest(root, session_id, &manifest)?;
    let file = index::fingerprint(event_path)?;
    let new_spans = transcript.apply_appended_event(event);
    let new_entries = input_history.apply_events(std::slice::from_ref(event));
    transcript.file = file.clone();
    transcript.event_count = transcript.event_count.saturating_add(1);
    transcript.end_offset = entry.offset.saturating_add(entry.frame_len);
    transcript.last_sequence = Some(event.sequence);
    input_history.file = file.clone();
    input_history.event_count = input_history.event_count.saturating_add(1);
    input_history.end_offset = entry.offset.saturating_add(entry.frame_len);
    input_history.last_sequence = Some(event.sequence);
    append_transcript_spans(root, session_id, &new_spans)?;
    append_input_history_entries(root, session_id, &new_entries)?;
    write_manifest(root, session_id, file, &transcript, &input_history)
}

pub fn load_stale_input_history_entries(
    root: &Path,
    session_id: SessionId,
) -> Result<Vec<SessionInputHistoryEntry>, SessionStoreError> {
    let index = load_input_history_index_lenient(root, session_id).map_err(load_error_to_store)?;
    Ok(index.entries)
}

pub fn ensure_input_history_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<InputHistoryIndex, SessionStoreError> {
    let manifest =
        ensure_index_current(root, session_id, event_path, DerivedIndexKind::InputHistory)?;
    let (_transcript, input_history) = load_indexes_from_manifest(root, session_id, &manifest)?;
    Ok(input_history)
}

/// Load a fresh transcript index, incrementally catching it up when stale.
pub fn ensure_transcript_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<TranscriptIndex, SessionStoreError> {
    let manifest =
        ensure_index_current(root, session_id, event_path, DerivedIndexKind::Transcript)?;
    let (mut transcript, input_history) = load_indexes_from_manifest(root, session_id, &manifest)?;
    if transcript.has_pending_streams() {
        let new_spans = transcript.flush_pending_streams();
        append_transcript_spans(root, session_id, &new_spans)?;
        let file = index::fingerprint(event_path)?;
        write_manifest(root, session_id, file, &transcript, &input_history)?;
    }
    Ok(transcript)
}

fn ensure_index_current(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
    kind: DerivedIndexKind,
) -> Result<DerivedIndexManifest, SessionStoreError> {
    let current_file = index::fingerprint(event_path)?;
    let manifest = match load_manifest(root, session_id) {
        Ok(manifest) => manifest,
        Err(_error) => {
            return Err(SessionStoreError::InvalidSessionId(
                "derived manifest missing; explicit repair is required".to_owned(),
            ));
        }
    };
    if validate_manifest(root, &manifest, session_id).is_err() {
        return Err(SessionStoreError::InvalidSessionId(
            "derived manifest requires repair".to_owned(),
        ));
    }
    if !can_resume_manifest(&manifest, current_file.len) {
        return Err(SessionStoreError::InvalidSessionId(
            "derived manifest checkpoint is beyond the event log".to_owned(),
        ));
    }
    if manifest_entry_is_current(&manifest, kind, current_file.len) {
        return Ok(manifest);
    }

    catch_up_manifest(root, session_id, event_path, &manifest, current_file, kind)
}

fn catch_up_manifest(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
    manifest: &DerivedIndexManifest,
    current_file: EventFileFingerprint,
    kind: DerivedIndexKind,
) -> Result<DerivedIndexManifest, SessionStoreError> {
    let (mut transcript, mut input_history) =
        load_indexes_from_manifest(root, session_id, manifest)?;
    let checkpoint = manifest_entry(manifest, kind)
        .map_err(|error| invalid_to_store(&error))?
        .checkpoint
        .clone();
    let tail_len = current_file.len.saturating_sub(checkpoint.end_offset);
    if tail_len > MAX_INLINE_DERIVED_CATCH_UP_BYTES {
        return Err(SessionStoreError::InvalidSessionId(
            "derived index tail is too large for normal attach; run session repair/reindex"
                .to_owned(),
        ));
    }
    let tail = read_tail_events(event_path, checkpoint.end_offset)?;
    if tail.len() > MAX_INLINE_DERIVED_CATCH_UP_EVENTS {
        return Err(SessionStoreError::InvalidSessionId(
            "derived index tail has too many events for normal attach; run session repair/reindex"
                .to_owned(),
        ));
    }
    let end_offset = current_file.len;
    let event_count = checkpoint.event_count.saturating_add(tail.len());
    let last_sequence = tail
        .last()
        .map_or(checkpoint.last_sequence, |event| Some(event.sequence));
    let mut new_spans = Vec::new();
    let mut new_entries = Vec::new();
    match kind {
        DerivedIndexKind::Transcript => {
            new_spans = transcript.apply_events(&tail);
            transcript.file = current_file.clone();
            transcript.event_count = event_count;
            transcript.end_offset = end_offset;
            transcript.last_sequence = last_sequence;
        }
        DerivedIndexKind::InputHistory => {
            new_entries = input_history.apply_events(&tail);
            input_history.file = current_file.clone();
            input_history.event_count = event_count;
            input_history.end_offset = end_offset;
            input_history.last_sequence = last_sequence;
        }
    }
    append_transcript_spans(root, session_id, &new_spans)?;
    append_input_history_entries(root, session_id, &new_entries)?;
    write_manifest(root, session_id, current_file, &transcript, &input_history)
}

fn manifest_entry_is_current(
    manifest: &DerivedIndexManifest,
    kind: DerivedIndexKind,
    file_len: u64,
) -> bool {
    manifest_entry(manifest, kind).is_ok_and(|entry| entry.checkpoint.end_offset == file_len)
}

fn manifest_is_current(manifest: &DerivedIndexManifest, file_len: u64) -> bool {
    manifest
        .indexes
        .iter()
        .all(|entry| entry.checkpoint.end_offset == file_len)
}

impl InputHistoryIndex {
    fn apply_events(&mut self, events: &[SessionEvent]) -> Vec<SessionInputHistoryEntry> {
        let new_entries = events
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
            .collect::<Vec<_>>();
        self.entries.extend(new_entries.clone());
        new_entries
    }
}

impl TranscriptIndex {
    fn apply_events(
        &mut self,
        events: &[SessionEvent],
    ) -> Vec<index::TranscriptProjectionIndexEntry> {
        let (mut spans, state) =
            project_transcript_events(events, Some(self.projector_state.clone()));
        let new_spans = spans.clone();
        self.spans.append(&mut spans);
        self.spans
            .sort_by_key(|span| span.source_range.start_sequence);
        self.projector_state = state;
        new_spans
    }

    fn apply_appended_event(
        &mut self,
        event: &SessionEvent,
    ) -> Vec<index::TranscriptProjectionIndexEntry> {
        let (mut spans, state) = project_transcript_events_incremental(
            std::slice::from_ref(event),
            Some(self.projector_state.clone()),
        );
        let new_spans = spans.clone();
        self.spans.append(&mut spans);
        self.spans
            .sort_by_key(|span| span.source_range.start_sequence);
        self.projector_state = state;
        new_spans
    }

    const fn has_pending_streams(&self) -> bool {
        self.projector_state.assistant.is_some() || self.projector_state.reasoning.is_some()
    }

    fn flush_pending_streams(&mut self) -> Vec<index::TranscriptProjectionIndexEntry> {
        let (mut spans, state) = project_transcript_events(&[], Some(self.projector_state.clone()));
        let new_spans = spans.clone();
        self.spans.append(&mut spans);
        self.spans
            .sort_by_key(|span| span.source_range.start_sequence);
        self.projector_state = state;
        new_spans
    }
}

fn can_resume_manifest(manifest: &DerivedIndexManifest, file_len: u64) -> bool {
    manifest
        .indexes
        .iter()
        .all(|entry| entry.checkpoint.end_offset <= file_len)
}

fn read_tail_events(
    event_path: &Path,
    offset: u64,
) -> Result<Vec<SessionEvent>, SessionStoreError> {
    let report = crate::reader::read_events_from_offset(event_path, offset)?;
    if !report.issues.is_empty() {
        return Err(SessionStoreError::InvalidSessionId(
            "derived catch-up tail read encountered an unreadable event frame".to_owned(),
        ));
    }
    Ok(report.events)
}

fn project_transcript_events(
    events: &[SessionEvent],
    initial_state: Option<TranscriptProjectorState>,
) -> (
    Vec<index::TranscriptProjectionIndexEntry>,
    TranscriptProjectorState,
) {
    let mut projector = TranscriptIncrementalProjector::new(initial_state.unwrap_or_default());
    for event in events {
        projector.apply(event);
    }
    projector.finish_batch()
}

fn project_transcript_events_incremental(
    events: &[SessionEvent],
    initial_state: Option<TranscriptProjectorState>,
) -> (
    Vec<index::TranscriptProjectionIndexEntry>,
    TranscriptProjectorState,
) {
    let mut projector = TranscriptIncrementalProjector::new(initial_state.unwrap_or_default());
    for event in events {
        projector.apply(event);
    }
    projector.finish_incremental()
}

struct TranscriptIncrementalProjector {
    state: TranscriptProjectorState,
    spans: Vec<index::TranscriptProjectionIndexEntry>,
}

impl TranscriptIncrementalProjector {
    const fn new(state: TranscriptProjectorState) -> Self {
        Self {
            state,
            spans: Vec::new(),
        }
    }

    fn apply(&mut self, event: &SessionEvent) {
        match &event.kind {
            SessionEventKind::AssistantDelta { text } => {
                self.push_stream_delta(
                    TranscriptProjectionItemKind::AssistantMessage,
                    event.sequence,
                    text.len(),
                );
            }
            SessionEventKind::AssistantMessage { text } => {
                self.finish_stream(
                    TranscriptProjectionItemKind::AssistantMessage,
                    event.sequence,
                    text.len(),
                );
            }
            SessionEventKind::AssistantReasoningDelta { text } => {
                self.push_stream_delta(
                    TranscriptProjectionItemKind::Reasoning,
                    event.sequence,
                    text.len(),
                );
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                self.finish_stream(
                    TranscriptProjectionItemKind::Reasoning,
                    event.sequence,
                    text.len(),
                );
            }
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                arguments_json,
                ..
            } => {
                self.flush_streams();
                self.state.tools.insert(
                    tool_call_id.clone(),
                    PendingToolState {
                        start_sequence: event.sequence,
                        end_sequence: event.sequence,
                        content_bytes: arguments_json.len(),
                        saw_stream_output: false,
                    },
                );
            }
            SessionEventKind::ToolInvocationStream { event: stream } => {
                self.flush_streams();
                self.apply_tool_stream(event.sequence, stream);
            }
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                ..
            } => {
                self.flush_streams();
                self.finish_tool_invocation(tool_call_id, event.sequence, result.len());
            }
            SessionEventKind::RuntimeWorkStarted { .. }
            | SessionEventKind::RuntimeWorkCancelRequested { .. }
            | SessionEventKind::RuntimeWorkProgress { .. }
            | SessionEventKind::RuntimeWorkFinished { .. }
            | SessionEventKind::ModelUsage { .. } => {
                if let Some((kind, bytes)) = non_streaming_transcript_item(event) {
                    self.push_span(kind, event.sequence, event.sequence, bytes);
                }
            }
            _ => {
                self.flush_streams();
                if let Some((kind, bytes)) = non_streaming_transcript_item(event) {
                    self.push_span(kind, event.sequence, event.sequence, bytes);
                }
            }
        }
    }

    fn finish_batch(
        mut self,
    ) -> (
        Vec<index::TranscriptProjectionIndexEntry>,
        TranscriptProjectorState,
    ) {
        self.flush_streams();
        self.finish_incremental()
    }

    fn finish_incremental(
        mut self,
    ) -> (
        Vec<index::TranscriptProjectionIndexEntry>,
        TranscriptProjectorState,
    ) {
        self.spans.sort_by_key(|span| {
            (
                span.source_range.start_sequence,
                span.source_range.end_sequence,
            )
        });
        (self.spans, self.state)
    }

    fn push_stream_delta(
        &mut self,
        kind: TranscriptProjectionItemKind,
        sequence: u64,
        bytes: usize,
    ) {
        let slot = self.pending_stream_slot(kind);
        if let Some(stream) = slot {
            stream.end_sequence = sequence;
            stream.content_bytes = stream.content_bytes.saturating_add(bytes);
        } else {
            *self.pending_stream_slot(kind) = Some(PendingStreamState {
                kind,
                start_sequence: sequence,
                end_sequence: sequence,
                content_bytes: bytes,
            });
        }
    }

    fn finish_stream(&mut self, kind: TranscriptProjectionItemKind, sequence: u64, bytes: usize) {
        let start_sequence = self
            .pending_stream_slot(kind)
            .take()
            .map_or(sequence, |stream| stream.start_sequence);
        self.push_span(kind, start_sequence, sequence, bytes);
    }

    fn flush_streams(&mut self) {
        let assistant = self.state.assistant.take();
        let reasoning = self.state.reasoning.take();
        if let Some(stream) = assistant {
            self.push_span(
                stream.kind,
                stream.start_sequence,
                stream.end_sequence,
                stream.content_bytes,
            );
        }
        if let Some(stream) = reasoning {
            self.push_span(
                stream.kind,
                stream.start_sequence,
                stream.end_sequence,
                stream.content_bytes,
            );
        }
    }

    fn apply_tool_stream(&mut self, sequence: u64, event: &ToolInvocationStreamEvent) {
        let tool_call_id = tool_stream_tool_call_id(event).to_owned();
        let content_bytes = tool_stream_content_bytes(event);
        let entry = self
            .state
            .tools
            .entry(tool_call_id)
            .or_insert(PendingToolState {
                start_sequence: sequence,
                end_sequence: sequence,
                content_bytes: 0,
                saw_stream_output: false,
            });
        entry.end_sequence = sequence;
        entry.content_bytes = entry.content_bytes.saturating_add(content_bytes);
        if !matches!(event, ToolInvocationStreamEvent::Started { .. }) {
            entry.saw_stream_output = true;
        }
    }

    fn finish_tool_invocation(&mut self, tool_call_id: &str, sequence: u64, result_bytes: usize) {
        if let Some(mut invocation) = self.state.tools.remove(tool_call_id) {
            invocation.end_sequence = sequence;
            if !invocation.saw_stream_output {
                invocation.content_bytes = invocation.content_bytes.saturating_add(result_bytes);
            }
            self.push_span(
                TranscriptProjectionItemKind::ToolInvocation,
                invocation.start_sequence,
                invocation.end_sequence,
                invocation.content_bytes,
            );
        } else {
            self.push_span(
                TranscriptProjectionItemKind::ToolInvocation,
                sequence,
                sequence,
                result_bytes,
            );
        }
    }

    fn push_span(
        &mut self,
        kind: TranscriptProjectionItemKind,
        start_sequence: u64,
        end_sequence: u64,
        content_bytes: usize,
    ) {
        self.spans.push(index::TranscriptProjectionIndexEntry {
            projection_item_id: format!("transcript:{start_sequence}:{end_sequence}"),
            kind,
            source_range: ProjectionSourceRange {
                start_sequence,
                end_sequence,
            },
            content_bytes,
        });
    }

    fn pending_stream_slot(
        &mut self,
        kind: TranscriptProjectionItemKind,
    ) -> &mut Option<PendingStreamState> {
        match kind {
            TranscriptProjectionItemKind::AssistantMessage => &mut self.state.assistant,
            TranscriptProjectionItemKind::Reasoning => &mut self.state.reasoning,
            _ => unreachable!("only streaming transcript item kinds have pending slots"),
        }
    }
}

fn non_streaming_transcript_item(
    event: &SessionEvent,
) -> Option<(TranscriptProjectionItemKind, usize)> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } | SessionEventKind::SystemMessage { text } => {
            Some((TranscriptProjectionItemKind::UserMessage, text.len()))
        }
        SessionEventKind::PermissionRequested { arguments_json, .. } => Some((
            TranscriptProjectionItemKind::Permission,
            arguments_json.len(),
        )),
        SessionEventKind::PermissionResolved { .. } => {
            Some((TranscriptProjectionItemKind::Permission, 0))
        }
        SessionEventKind::ContextCompacted { summary, .. } => Some((
            TranscriptProjectionItemKind::ContextCompaction,
            summary.len(),
        )),
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } => Some((
            TranscriptProjectionItemKind::WorkingDirectoryChange,
            old_working_directory.as_os_str().len() + new_working_directory.as_os_str().len(),
        )),
        SessionEventKind::SkillInvoked { arguments, .. } => {
            Some((TranscriptProjectionItemKind::Other, arguments.len()))
        }
        SessionEventKind::SkillInvocationFailed { error, .. } => {
            Some((TranscriptProjectionItemKind::Other, error.len()))
        }
        SessionEventKind::ModelUsage { .. } => Some((TranscriptProjectionItemKind::Other, 0)),
        _ => None,
    }
}

fn tool_stream_tool_call_id(event: &ToolInvocationStreamEvent) -> &str {
    match event {
        ToolInvocationStreamEvent::Started { tool_call_id, .. }
        | ToolInvocationStreamEvent::OutputDelta { tool_call_id, .. }
        | ToolInvocationStreamEvent::Status { tool_call_id, .. }
        | ToolInvocationStreamEvent::Finished { tool_call_id, .. } => tool_call_id,
    }
}

const fn tool_stream_content_bytes(event: &ToolInvocationStreamEvent) -> usize {
    match event {
        ToolInvocationStreamEvent::Started { tool_name, .. } => tool_name.len(),
        ToolInvocationStreamEvent::OutputDelta { text, .. }
        | ToolInvocationStreamEvent::Status { message: text, .. } => text.len(),
        ToolInvocationStreamEvent::Finished { .. } => 0,
    }
}

fn remove_legacy_derived_state(
    root: &Path,
    session_id: SessionId,
) -> Result<(), SessionStoreError> {
    for file_name in ["dirty.json", "transcript_state.json"] {
        let path = session_index_dir(root, session_id).join(file_name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(SessionStoreError::Io(error)),
        }
    }
    Ok(())
}

fn load_input_history_index_lenient(
    root: &Path,
    session_id: SessionId,
) -> Result<InputHistoryIndex, DerivedIndexLoadError> {
    load_input_history_index_inner(root, session_id, None)
}

fn load_transcript_index_lenient(
    root: &Path,
    session_id: SessionId,
) -> Result<TranscriptIndex, TranscriptIndexLoadError> {
    load_transcript_index_inner(root, session_id, None)
}

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

fn append_input_history_entries(
    root: &Path,
    session_id: SessionId,
    entries: &[SessionInputHistoryEntry],
) -> Result<(), SessionStoreError> {
    if entries.is_empty() {
        return Ok(());
    }
    let path = input_history_index_path(root, session_id);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(SessionStoreError::Io)?;
    for entry in entries {
        serde_json::to_writer(&mut file, entry).map_err(SessionStoreError::Index)?;
        writeln!(file).map_err(SessionStoreError::Io)?;
    }
    file.flush().map_err(SessionStoreError::Io)
}

fn append_transcript_spans(
    root: &Path,
    session_id: SessionId,
    spans: &[index::TranscriptProjectionIndexEntry],
) -> Result<(), SessionStoreError> {
    if spans.is_empty() {
        return Ok(());
    }
    let path = transcript_index_path(root, session_id);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(SessionStoreError::Io)?;
    for span in spans {
        serde_json::to_writer(&mut file, span).map_err(SessionStoreError::Index)?;
        writeln!(file).map_err(SessionStoreError::Io)?;
    }
    file.flush().map_err(SessionStoreError::Io)
}

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

/// Persist a derived-index manifest atomically.
pub fn write_manifest(
    root: &Path,
    session_id: SessionId,
    file: EventFileFingerprint,
    transcript_index: &TranscriptIndex,
    input_index: &InputHistoryIndex,
) -> Result<DerivedIndexManifest, SessionStoreError> {
    let manifest = DerivedIndexManifest {
        manifest_version: DERIVED_INDEX_MANIFEST_VERSION,
        session_id,
        file,
        indexes: vec![
            DerivedIndexManifestEntry {
                id: DerivedIndexKind::Transcript.id().to_owned(),
                version: DerivedIndexKind::Transcript.current_version(),
                path: "transcript.jsonl".to_owned(),
                checkpoint: checkpoint_from_transcript(transcript_index),
                transcript_state: Some(transcript_index.projector_state.clone()),
                item_count: transcript_index.spans.len(),
            },
            DerivedIndexManifestEntry {
                id: DerivedIndexKind::InputHistory.id().to_owned(),
                version: DerivedIndexKind::InputHistory.current_version(),
                path: "input_history.jsonl".to_owned(),
                checkpoint: checkpoint_from_input_history(input_index),
                transcript_state: None,
                item_count: input_index.entries.len(),
            },
        ],
    };
    persist_manifest(root, &manifest)?;
    Ok(manifest)
}

const fn checkpoint_from_transcript(index: &TranscriptIndex) -> DerivedSourceCheckpoint {
    DerivedSourceCheckpoint {
        event_count: index.event_count,
        end_offset: index.end_offset,
        last_sequence: index.last_sequence,
    }
}

const fn checkpoint_from_input_history(index: &InputHistoryIndex) -> DerivedSourceCheckpoint {
    DerivedSourceCheckpoint {
        event_count: index.event_count,
        end_offset: index.end_offset,
        last_sequence: index.last_sequence,
    }
}

fn persist_manifest(root: &Path, manifest: &DerivedIndexManifest) -> Result<(), SessionStoreError> {
    let path = manifest_path(root, manifest.session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(SessionStoreError::Io)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let file = File::create(&tmp_path).map_err(SessionStoreError::Io)?;
    serde_json::to_writer_pretty(file, manifest).map_err(SessionStoreError::Index)?;
    fs::rename(&tmp_path, &path).map_err(SessionStoreError::Io)
}

#[derive(Debug)]
enum ManifestLoadError {
    NotFound,
    Store,
}

fn load_manifest(
    root: &Path,
    session_id: SessionId,
) -> Result<DerivedIndexManifest, ManifestLoadError> {
    let path = manifest_path(root, session_id);
    let file = File::open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ManifestLoadError::NotFound
        } else {
            ManifestLoadError::Store
        }
    })?;
    serde_json::from_reader(file)
        .map_err(SessionStoreError::Index)
        .map_err(|_error| ManifestLoadError::Store)
}

fn validate_manifest(
    root: &Path,
    manifest: &DerivedIndexManifest,
    session_id: SessionId,
) -> Result<(), DerivedIndexInvalid> {
    validate_header(
        manifest.manifest_version,
        DERIVED_INDEX_MANIFEST_VERSION,
        manifest.session_id,
        session_id,
        None,
    )?;
    validate_manifest_entry(
        root,
        session_id,
        manifest,
        DerivedIndexKind::Transcript,
        "transcript.jsonl",
    )?;
    validate_manifest_entry(
        root,
        session_id,
        manifest,
        DerivedIndexKind::InputHistory,
        "input_history.jsonl",
    )?;
    for entry in &manifest.indexes {
        if entry.id != DerivedIndexKind::Transcript.id()
            && entry.id != DerivedIndexKind::InputHistory.id()
        {
            return Err(DerivedIndexInvalid::UnknownManifestEntry {
                id: entry.id.clone(),
            });
        }
    }
    Ok(())
}

fn validate_manifest_entry(
    root: &Path,
    session_id: SessionId,
    manifest: &DerivedIndexManifest,
    kind: DerivedIndexKind,
    expected_path: &str,
) -> Result<(), DerivedIndexInvalid> {
    let Some(entry) = manifest.indexes.iter().find(|entry| entry.id == kind.id()) else {
        return Err(DerivedIndexInvalid::MissingManifestEntry {
            id: kind.id().to_owned(),
        });
    };
    if entry.version != kind.current_version() {
        return Err(DerivedIndexInvalid::ManifestEntryVersion {
            id: entry.id.clone(),
            found: entry.version,
            current: kind.current_version(),
        });
    }
    if entry.path != expected_path {
        return Err(DerivedIndexInvalid::ManifestPath {
            id: entry.id.clone(),
            found: entry.path.clone(),
            expected: expected_path.to_owned(),
        });
    }
    if !session_index_dir(root, session_id)
        .join(&entry.path)
        .exists()
    {
        return Err(DerivedIndexInvalid::MissingManifestEntry {
            id: entry.id.clone(),
        });
    }
    Ok(())
}

fn validate_manifest_counts(
    manifest: &DerivedIndexManifest,
    transcript: &TranscriptIndex,
    input_history: &InputHistoryIndex,
) -> Result<(), DerivedIndexInvalid> {
    let transcript_entry = manifest_entry(manifest, DerivedIndexKind::Transcript)?;
    if transcript_entry.checkpoint.event_count != transcript.event_count
        || transcript_entry.checkpoint.end_offset != transcript.end_offset
        || transcript_entry.checkpoint.last_sequence != transcript.last_sequence
        || transcript_entry.item_count != transcript.spans.len()
    {
        return Err(DerivedIndexInvalid::ManifestEntryCount {
            id: transcript_entry.id.clone(),
        });
    }
    let input_entry = manifest_entry(manifest, DerivedIndexKind::InputHistory)?;
    if input_entry.checkpoint.event_count != input_history.event_count
        || input_entry.checkpoint.end_offset != input_history.end_offset
        || input_entry.checkpoint.last_sequence != input_history.last_sequence
        || input_entry.item_count != input_history.entries.len()
    {
        return Err(DerivedIndexInvalid::ManifestEntryCount {
            id: input_entry.id.clone(),
        });
    }
    Ok(())
}

fn manifest_entry(
    manifest: &DerivedIndexManifest,
    kind: DerivedIndexKind,
) -> Result<&DerivedIndexManifestEntry, DerivedIndexInvalid> {
    manifest
        .indexes
        .iter()
        .find(|entry| entry.id == kind.id())
        .ok_or_else(|| DerivedIndexInvalid::MissingManifestEntry {
            id: kind.id().to_owned(),
        })
}

fn load_indexes_from_manifest(
    root: &Path,
    session_id: SessionId,
    manifest: &DerivedIndexManifest,
) -> Result<(TranscriptIndex, InputHistoryIndex), SessionStoreError> {
    validate_manifest(root, manifest, session_id).map_err(|error| invalid_to_store(&error))?;
    let mut transcript =
        load_transcript_index_lenient(root, session_id).map_err(transcript_load_error_to_store)?;
    let mut input_history =
        load_input_history_index_lenient(root, session_id).map_err(load_error_to_store)?;
    apply_manifest_metadata(&mut transcript, &mut input_history, manifest)?;
    validate_manifest_counts(manifest, &transcript, &input_history)
        .map_err(|error| invalid_to_store(&error))?;
    Ok((transcript, input_history))
}

fn apply_manifest_metadata(
    transcript: &mut TranscriptIndex,
    input_history: &mut InputHistoryIndex,
    manifest: &DerivedIndexManifest,
) -> Result<(), SessionStoreError> {
    let transcript_entry = manifest_entry(manifest, DerivedIndexKind::Transcript)
        .map_err(|error| invalid_to_store(&error))?;
    let input_entry = manifest_entry(manifest, DerivedIndexKind::InputHistory)
        .map_err(|error| invalid_to_store(&error))?;
    transcript.event_count = transcript_entry.checkpoint.event_count;
    transcript.end_offset = transcript_entry.checkpoint.end_offset;
    transcript.last_sequence = transcript_entry.checkpoint.last_sequence;
    transcript.projector_state = transcript_entry
        .transcript_state
        .clone()
        .unwrap_or_default();
    input_history.event_count = input_entry.checkpoint.event_count;
    input_history.end_offset = input_entry.checkpoint.end_offset;
    input_history.last_sequence = input_entry.checkpoint.last_sequence;
    transcript.file = manifest.file.clone();
    input_history.file = manifest.file.clone();
    Ok(())
}

fn invalid_to_store(error: &DerivedIndexInvalid) -> SessionStoreError {
    SessionStoreError::InvalidSessionId(format!("invalid derived index: {error:?}"))
}

fn health_from_manifest(manifest: &DerivedIndexManifest, rebuilt: bool) -> DerivedHealth {
    DerivedHealth {
        manifest: DerivedManifestHealth {
            fresh: true,
            rebuilt,
            issue: None,
        },
        indexes: manifest
            .indexes
            .iter()
            .filter_map(|entry| {
                let kind = match entry.id.as_str() {
                    "transcript" => DerivedIndexKind::Transcript,
                    "input_history" => DerivedIndexKind::InputHistory,
                    _ => return None,
                };
                Some(DerivedIndexHealth {
                    kind,
                    fresh: true,
                    rebuilt,
                    event_count: Some(entry.checkpoint.event_count),
                    item_count: Some(entry.item_count),
                    issue: None,
                })
            })
            .collect(),
    }
}

fn issue_for_load_error(error: &impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

fn load_error_to_store(error: DerivedIndexLoadError) -> SessionStoreError {
    match error {
        DerivedIndexLoadError::Store(error) => error,
        DerivedIndexLoadError::NotFound => {
            SessionStoreError::InvalidSessionId("missing input history index".to_owned())
        }
        DerivedIndexLoadError::Invalid => {
            SessionStoreError::InvalidSessionId("invalid input history index".to_owned())
        }
    }
}

fn transcript_load_error_to_store(error: TranscriptIndexLoadError) -> SessionStoreError {
    match error {
        TranscriptIndexLoadError::Store(error) => error,
        TranscriptIndexLoadError::NotFound => {
            SessionStoreError::InvalidSessionId("missing transcript index".to_owned())
        }
        TranscriptIndexLoadError::Invalid => {
            SessionStoreError::InvalidSessionId("invalid transcript index".to_owned())
        }
    }
}

fn load_input_history_index_inner(
    root: &Path,
    session_id: SessionId,
    file: Option<&EventFileFingerprint>,
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
        end_offset: header.end_offset,
        last_sequence: header.last_sequence,
        entries,
    };
    input_index
        .validate(session_id, file)
        .map_err(|_error| DerivedIndexLoadError::Invalid)?;
    Ok(input_index)
}

fn load_transcript_index_inner(
    root: &Path,
    session_id: SessionId,
    file: Option<&EventFileFingerprint>,
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
        end_offset: header.end_offset,
        last_sequence: header.last_sequence,
        projector_state: header.projector_state,
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
    end_offset: u64,
    last_sequence: Option<u64>,
    projector_state: TranscriptProjectorState,
}

impl From<&TranscriptIndex> for TranscriptIndexHeader {
    fn from(value: &TranscriptIndex) -> Self {
        Self {
            index_version: value.index_version,
            session_id: value.session_id,
            file: value.file.clone(),
            event_count: value.event_count,
            end_offset: value.end_offset,
            last_sequence: value.last_sequence,
            projector_state: value.projector_state.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct InputHistoryIndexHeader {
    index_version: u16,
    session_id: SessionId,
    file: EventFileFingerprint,
    event_count: usize,
    end_offset: u64,
    last_sequence: Option<u64>,
}

impl From<&InputHistoryIndex> for InputHistoryIndexHeader {
    fn from(value: &InputHistoryIndex) -> Self {
        Self {
            index_version: value.index_version,
            session_id: value.session_id,
            file: value.file.clone(),
            event_count: value.event_count,
            end_offset: value.end_offset,
            last_sequence: value.last_sequence,
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
