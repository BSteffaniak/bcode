//! Derived session index files and validation.

use crate::index::{self, EventFileFingerprint};
use crate::{SessionStoreError, projection};
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
pub const DERIVED_INDEX_MANIFEST_VERSION: u16 = 1;

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
    /// Number of source events covered by the index.
    pub event_count: usize,
    /// Number of derived records in the index.
    pub item_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TranscriptPendingState {
    session_id: SessionId,
    assistant: Option<PendingStreamState>,
    reasoning: Option<PendingStreamState>,
    tools: BTreeMap<String, PendingToolState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingStreamState {
    start_sequence: u64,
    end_sequence: u64,
    content_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingToolState {
    start_sequence: u64,
    end_sequence: u64,
    content_bytes: usize,
    saw_stream_output: bool,
}

impl TranscriptPendingState {
    const fn empty(session_id: SessionId) -> Self {
        Self {
            session_id,
            assistant: None,
            reasoning: None,
            tools: BTreeMap::new(),
        }
    }

    fn pending_spans(&self) -> Vec<index::TranscriptProjectionIndexEntry> {
        let mut spans = Vec::new();
        if let Some(stream) = &self.assistant {
            spans.push(stream.to_span(TranscriptProjectionItemKind::AssistantMessage));
        }
        if let Some(stream) = &self.reasoning {
            spans.push(stream.to_span(TranscriptProjectionItemKind::Reasoning));
        }
        spans.extend(self.tools.values().map(PendingToolState::to_span));
        spans.sort_by_key(|span| {
            (
                span.source_range.start_sequence,
                span.source_range.end_sequence,
            )
        });
        spans
    }
}

impl PendingStreamState {
    fn to_span(&self, kind: TranscriptProjectionItemKind) -> index::TranscriptProjectionIndexEntry {
        transcript_span_range(
            self.start_sequence,
            self.end_sequence,
            kind,
            self.content_bytes,
        )
    }
}

impl PendingToolState {
    fn to_span(&self) -> index::TranscriptProjectionIndexEntry {
        transcript_span_range(
            self.start_sequence,
            self.end_sequence,
            TranscriptProjectionItemKind::ToolInvocation,
            self.content_bytes,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DerivedDirtyMarker {
    session_id: SessionId,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputHistoryIndex {
    /// Index format version.
    pub index_version: u16,
    /// Indexed session id.
    pub session_id: SessionId,
    /// Event-file fingerprint this index was built from when fully rebuilt.
    ///
    /// Incremental appends may advance `event_count` without rewriting this
    /// fingerprint; freshness is enforced by the primary metadata index and
    /// manifest for hot paths, while doctor/reindex rewrites a fully fresh
    /// fingerprint.
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
        file: Option<&EventFileFingerprint>,
    ) -> Result<(), DerivedIndexInvalid> {
        validate_header(
            self.index_version,
            INPUT_HISTORY_INDEX_VERSION,
            self.session_id,
            session_id,
            file.map(|file| (&self.file, file)),
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
        file: Option<&EventFileFingerprint>,
    ) -> Result<(), DerivedIndexInvalid> {
        validate_header(
            self.index_version,
            TRANSCRIPT_INDEX_VERSION,
            self.session_id,
            session_id,
            file.map(|file| (&self.file, file)),
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
    fingerprint: Option<(&EventFileFingerprint, &EventFileFingerprint)>,
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
    if let Some((found_file, expected_file)) = fingerprint
        && found_file != expected_file
    {
        return Err(DerivedIndexInvalid::Fingerprint);
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

fn transcript_state_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_index_dir(root, session_id).join("transcript_state.json")
}

fn dirty_marker_path(root: &Path, session_id: SessionId) -> PathBuf {
    session_index_dir(root, session_id).join("dirty.json")
}

/// Rebuild all registered derived indexes for a session.
pub fn rebuild_all(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<DerivedIndexManifest, SessionStoreError> {
    let report = crate::reader::read_events(event_path)?;
    let file = index::fingerprint(event_path)?;
    let transcript_index = TranscriptIndex::from_events(session_id, file.clone(), &report.events);
    let input_index = InputHistoryIndex::from_events(session_id, file.clone(), &report.events);
    write_transcript_index(root, &transcript_index)?;
    write_input_history_index(root, &input_index)?;
    write_manifest(root, session_id, file, &transcript_index, &input_index)
}

/// Return health for all registered derived indexes, optionally rebuilding stale indexes.
pub fn health_all(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
    fix: bool,
    force: bool,
) -> Result<DerivedHealth, SessionStoreError> {
    if force && fix {
        let manifest = rebuild_all(root, session_id, event_path)?;
        return Ok(health_from_manifest(&manifest, true));
    }
    let file = index::fingerprint(event_path)?;
    match validate_all(root, session_id, &file) {
        Ok(manifest) => Ok(health_from_manifest(&manifest, false)),
        Err(issue) if fix => {
            let manifest = rebuild_all(root, session_id, event_path)?;
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
    validate_manifest(root, &manifest, session_id, Some(file))
        .map_err(|error| format!("{error:?}"))?;
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

pub fn ensure_input_history_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<InputHistoryIndex, SessionStoreError> {
    let file = index::fingerprint(event_path)?;
    if let Ok(manifest) = load_manifest(root, session_id)
        && validate_manifest(root, &manifest, session_id, Some(&file)).is_ok()
        && let Ok((_transcript, input_history)) =
            load_indexes_from_manifest(root, session_id, &manifest)
    {
        return Ok(input_history);
    }

    mark_dirty(
        root,
        session_id,
        "input-history derived index is missing or stale".to_owned(),
    )?;
    match load_input_history_index_lenient(root, session_id) {
        Ok(mut input_history) => {
            input_history.file = file;
            Ok(input_history)
        }
        Err(_error) => Ok(InputHistoryIndex {
            index_version: INPUT_HISTORY_INDEX_VERSION,
            session_id,
            file,
            event_count: 0,
            entries: Vec::new(),
        }),
    }
}

/// Load a fresh transcript index, rebuilding when it is absent or stale.
pub fn ensure_transcript_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<TranscriptIndex, SessionStoreError> {
    let file = index::fingerprint(event_path)?;
    if let Ok(manifest) = load_manifest(root, session_id)
        && validate_manifest(root, &manifest, session_id, Some(&file)).is_ok()
        && let Ok((mut transcript, _input_history)) =
            load_indexes_from_manifest(root, session_id, &manifest)
    {
        overlay_pending_transcript_state(root, &mut transcript)?;
        return Ok(transcript);
    }

    mark_dirty(
        root,
        session_id,
        "transcript derived index is missing or stale".to_owned(),
    )?;
    Err(SessionStoreError::InvalidSessionId(
        "transcript derived index requires reindex".to_owned(),
    ))
}

/// Incrementally maintain derived indexes after one appended event.
pub fn append_event(
    root: &Path,
    event: &SessionEvent,
    file: EventFileFingerprint,
    event_count: usize,
) -> Result<(), SessionStoreError> {
    let manifest = match load_manifest(root, event.session_id) {
        Ok(manifest) => manifest,
        Err(_error) if event_count == 1 => {
            initialize_from_first_event(root, event, file)?;
            return Ok(());
        }
        Err(error) => {
            mark_dirty(
                root,
                event.session_id,
                format!("missing derived manifest: {error:?}"),
            )?;
            return Ok(());
        }
    };
    let previous_event_count = event_count.saturating_sub(1);
    if validate_manifest(root, &manifest, event.session_id, None).is_err()
        || !manifest_covers_event_count(&manifest, previous_event_count)
    {
        mark_dirty(
            root,
            event.session_id,
            "derived manifest does not match append predecessor".to_owned(),
        )?;
        return Ok(());
    }

    let (mut transcript, mut input) =
        match load_indexes_from_manifest(root, event.session_id, &manifest) {
            Ok(indexes) => indexes,
            Err(error) => {
                mark_dirty(
                    root,
                    event.session_id,
                    format!("derived sidecar load failed: {error}"),
                )?;
                return Ok(());
            }
        };

    if event_affects_transcript(event) {
        apply_transcript_append(root, &mut transcript, event)?;
    }
    if let SessionEventKind::UserMessage { text, .. } = &event.kind {
        append_input_history_entry(
            root,
            &mut input,
            SessionInputHistoryEntry {
                sequence: event.sequence,
                text: text.clone(),
            },
        )?;
    }
    transcript.event_count = event_count;
    input.event_count = event_count;
    transcript.file = file.clone();
    input.file = file.clone();
    write_manifest(root, event.session_id, file, &transcript, &input).map(|_| ())
}

fn initialize_from_first_event(
    root: &Path,
    event: &SessionEvent,
    file: EventFileFingerprint,
) -> Result<(), SessionStoreError> {
    let events = std::slice::from_ref(event);
    let transcript = TranscriptIndex::from_events(event.session_id, file.clone(), events);
    let input = InputHistoryIndex::from_events(event.session_id, file.clone(), events);
    write_transcript_index(root, &transcript)?;
    write_input_history_index(root, &input)?;
    write_manifest(root, event.session_id, file, &transcript, &input).map(|_| ())
}

fn event_affects_transcript(event: &SessionEvent) -> bool {
    matches!(
        &event.kind,
        SessionEventKind::AssistantDelta { .. }
            | SessionEventKind::AssistantMessage { .. }
            | SessionEventKind::AssistantReasoningDelta { .. }
            | SessionEventKind::AssistantReasoningMessage { .. }
            | SessionEventKind::ToolCallRequested { .. }
            | SessionEventKind::ToolInvocationStream { .. }
            | SessionEventKind::ToolCallFinished { .. }
    ) || non_streaming_transcript_item(event).is_some()
}

fn manifest_covers_event_count(manifest: &DerivedIndexManifest, event_count: usize) -> bool {
    manifest
        .indexes
        .iter()
        .all(|entry| entry.event_count == event_count)
}

fn apply_transcript_append(
    root: &Path,
    index: &mut TranscriptIndex,
    event: &SessionEvent,
) -> Result<(), SessionStoreError> {
    let mut state = load_transcript_state(root, event.session_id)?;
    let state_changed = match &event.kind {
        SessionEventKind::AssistantDelta { text } => {
            update_pending_stream(&mut state.assistant, event.sequence, text.len());
            true
        }
        SessionEventKind::AssistantMessage { text } => {
            finalize_pending_stream(
                root,
                index,
                &mut state.assistant,
                event.sequence,
                TranscriptProjectionItemKind::AssistantMessage,
                text.len(),
            )?;
            true
        }
        SessionEventKind::AssistantReasoningDelta { text } => {
            update_pending_stream(&mut state.reasoning, event.sequence, text.len());
            true
        }
        SessionEventKind::AssistantReasoningMessage { text } => {
            finalize_pending_stream(
                root,
                index,
                &mut state.reasoning,
                event.sequence,
                TranscriptProjectionItemKind::Reasoning,
                text.len(),
            )?;
            true
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            arguments_json,
            ..
        } => {
            state.tools.insert(
                tool_call_id.clone(),
                PendingToolState {
                    start_sequence: event.sequence,
                    end_sequence: event.sequence,
                    content_bytes: arguments_json.len(),
                    saw_stream_output: false,
                },
            );
            true
        }
        SessionEventKind::ToolInvocationStream { event: stream } => {
            update_pending_tool_stream(&mut state, event.sequence, stream);
            true
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            ..
        } => {
            finalize_pending_tool(
                root,
                index,
                &mut state,
                tool_call_id,
                event.sequence,
                result.len(),
            )?;
            true
        }
        _ => {
            if let Some((kind, content_bytes)) = non_streaming_transcript_item(event) {
                append_transcript_span(
                    root,
                    index,
                    transcript_span(event.sequence, kind, content_bytes),
                )?;
            }
            false
        }
    };
    if state_changed {
        persist_transcript_state(root, &state)?;
    }
    Ok(())
}

fn finalize_pending_stream(
    root: &Path,
    index: &mut TranscriptIndex,
    stream: &mut Option<PendingStreamState>,
    sequence: u64,
    kind: TranscriptProjectionItemKind,
    content_bytes: usize,
) -> Result<(), SessionStoreError> {
    let start_sequence = stream
        .take()
        .map_or(sequence, |stream| stream.start_sequence);
    append_transcript_span(
        root,
        index,
        transcript_span_range(start_sequence, sequence, kind, content_bytes),
    )
}

fn update_pending_tool_stream(
    state: &mut TranscriptPendingState,
    sequence: u64,
    stream: &ToolInvocationStreamEvent,
) {
    let tool_call_id = tool_stream_tool_call_id(stream).to_owned();
    let tool = state.tools.entry(tool_call_id).or_insert(PendingToolState {
        start_sequence: sequence,
        end_sequence: sequence,
        content_bytes: 0,
        saw_stream_output: false,
    });
    tool.end_sequence = sequence;
    tool.content_bytes = tool
        .content_bytes
        .saturating_add(tool_stream_content_bytes(stream));
    if matches!(stream, ToolInvocationStreamEvent::OutputDelta { .. }) {
        tool.saw_stream_output = true;
    }
}

fn finalize_pending_tool(
    root: &Path,
    index: &mut TranscriptIndex,
    state: &mut TranscriptPendingState,
    tool_call_id: &str,
    sequence: u64,
    result_bytes: usize,
) -> Result<(), SessionStoreError> {
    let mut tool = state
        .tools
        .remove(tool_call_id)
        .unwrap_or(PendingToolState {
            start_sequence: sequence,
            end_sequence: sequence,
            content_bytes: 0,
            saw_stream_output: false,
        });
    tool.end_sequence = sequence;
    if !tool.saw_stream_output {
        tool.content_bytes = tool.content_bytes.saturating_add(result_bytes);
    }
    append_transcript_span(root, index, tool.to_span())
}

const fn update_pending_stream(
    stream: &mut Option<PendingStreamState>,
    sequence: u64,
    content_bytes: usize,
) {
    if let Some(stream) = stream {
        stream.end_sequence = sequence;
        stream.content_bytes = stream.content_bytes.saturating_add(content_bytes);
    } else {
        *stream = Some(PendingStreamState {
            start_sequence: sequence,
            end_sequence: sequence,
            content_bytes,
        });
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

fn transcript_span(
    sequence: u64,
    kind: TranscriptProjectionItemKind,
    content_bytes: usize,
) -> index::TranscriptProjectionIndexEntry {
    transcript_span_range(sequence, sequence, kind, content_bytes)
}

fn transcript_span_range(
    start_sequence: u64,
    end_sequence: u64,
    kind: TranscriptProjectionItemKind,
    content_bytes: usize,
) -> index::TranscriptProjectionIndexEntry {
    index::TranscriptProjectionIndexEntry {
        projection_item_id: format!("transcript:{start_sequence}:{end_sequence}"),
        kind,
        source_range: ProjectionSourceRange {
            start_sequence,
            end_sequence,
        },
        content_bytes,
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

fn overlay_pending_transcript_state(
    root: &Path,
    index: &mut TranscriptIndex,
) -> Result<(), SessionStoreError> {
    let state = load_transcript_state(root, index.session_id)?;
    index.spans.extend(state.pending_spans());
    index.spans.sort_by_key(|span| {
        (
            span.source_range.start_sequence,
            span.source_range.end_sequence,
        )
    });
    Ok(())
}

fn load_transcript_state(
    root: &Path,
    session_id: SessionId,
) -> Result<TranscriptPendingState, SessionStoreError> {
    let path = transcript_state_path(root, session_id);
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TranscriptPendingState::empty(session_id));
        }
        Err(error) => return Err(SessionStoreError::Io(error)),
    };
    let state: TranscriptPendingState =
        serde_json::from_reader(file).map_err(SessionStoreError::Index)?;
    if state.session_id == session_id {
        Ok(state)
    } else {
        Ok(TranscriptPendingState::empty(session_id))
    }
}

fn persist_transcript_state(
    root: &Path,
    state: &TranscriptPendingState,
) -> Result<(), SessionStoreError> {
    let path = transcript_state_path(root, state.session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(SessionStoreError::Io)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let file = File::create(&tmp_path).map_err(SessionStoreError::Io)?;
    serde_json::to_writer_pretty(file, state).map_err(SessionStoreError::Index)?;
    fs::rename(&tmp_path, &path).map_err(SessionStoreError::Io)
}

fn mark_dirty(root: &Path, session_id: SessionId, reason: String) -> Result<(), SessionStoreError> {
    let path = dirty_marker_path(root, session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(SessionStoreError::Io)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let file = File::create(&tmp_path).map_err(SessionStoreError::Io)?;
    serde_json::to_writer_pretty(file, &DerivedDirtyMarker { session_id, reason })
        .map_err(SessionStoreError::Index)?;
    fs::rename(&tmp_path, &path).map_err(SessionStoreError::Io)
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

fn append_input_history_entry(
    root: &Path,
    index: &mut InputHistoryIndex,
    entry: SessionInputHistoryEntry,
) -> Result<(), SessionStoreError> {
    let path = input_history_index_path(root, index.session_id);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(SessionStoreError::Io)?;
    serde_json::to_writer(&mut file, &entry).map_err(SessionStoreError::Index)?;
    writeln!(file).map_err(SessionStoreError::Io)?;
    file.flush().map_err(SessionStoreError::Io)?;
    index.entries.push(entry);
    Ok(())
}

fn append_transcript_span(
    root: &Path,
    index: &mut TranscriptIndex,
    span: index::TranscriptProjectionIndexEntry,
) -> Result<(), SessionStoreError> {
    let path = transcript_index_path(root, index.session_id);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(SessionStoreError::Io)?;
    serde_json::to_writer(&mut file, &span).map_err(SessionStoreError::Index)?;
    writeln!(file).map_err(SessionStoreError::Io)?;
    file.flush().map_err(SessionStoreError::Io)?;
    index.spans.push(span);
    Ok(())
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
                event_count: transcript_index.event_count,
                item_count: transcript_index.spans.len(),
            },
            DerivedIndexManifestEntry {
                id: DerivedIndexKind::InputHistory.id().to_owned(),
                version: DerivedIndexKind::InputHistory.current_version(),
                path: "input_history.jsonl".to_owned(),
                event_count: input_index.event_count,
                item_count: input_index.entries.len(),
            },
        ],
    };
    persist_manifest(root, &manifest)?;
    Ok(manifest)
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
    file: Option<&EventFileFingerprint>,
) -> Result<(), DerivedIndexInvalid> {
    validate_header(
        manifest.manifest_version,
        DERIVED_INDEX_MANIFEST_VERSION,
        manifest.session_id,
        session_id,
        file.map(|file| (&manifest.file, file)),
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
    if transcript_entry.event_count != transcript.event_count
        || transcript_entry.item_count != transcript.spans.len()
    {
        return Err(DerivedIndexInvalid::ManifestEntryCount {
            id: transcript_entry.id.clone(),
        });
    }
    let input_entry = manifest_entry(manifest, DerivedIndexKind::InputHistory)?;
    if input_entry.event_count != input_history.event_count
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
    validate_manifest(root, manifest, session_id, None)
        .map_err(|error| invalid_to_store(&error))?;
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
    transcript.event_count = transcript_entry.event_count;
    input_history.event_count = input_entry.event_count;
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
                    event_count: Some(entry.event_count),
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
