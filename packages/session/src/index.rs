use crate::reader::{SessionReadIssue, SessionReadIssueKind, SessionReadReport};
use crate::{SessionState, SessionStoreError};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId, SessionSummary};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

pub const SESSION_INDEX_VERSION: u16 = 5;
pub const SESSION_ENTRY_INDEX_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFileFingerprint {
    pub len: u64,
    pub modified_unix_secs: u64,
    pub modified_nanos: u32,
    pub created_unix_secs: u64,
    pub created_nanos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndex {
    pub index_version: u16,
    pub session_id: SessionId,
    pub file: EventFileFingerprint,
    pub summary: SessionSummary,
    pub next_sequence: u64,
    pub event_count: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub has_user_message: bool,
    pub last_good_offset: u64,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub current_agent: Option<String>,
    pub latest_compaction_sequence: Option<u64>,
    pub total_metered_tokens: u64,
    pub min_event_schema_version: Option<u16>,
    pub max_event_schema_version: Option<u16>,
    pub issues: Vec<SessionIndexIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexIssue {
    pub offset: u64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    pub entry_index_version: u16,
    pub sequence: u64,
    pub offset: u64,
    pub frame_len: u64,
    pub kind: String,
    pub schema_version: u16,
}

#[derive(Debug, Deserialize)]
struct RawIndexVersion {
    index_version: u16,
    session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionIndexStatus {
    Current(Box<SessionIndex>),
    Missing {
        current_version: u16,
    },
    Stale {
        found_version: Option<u16>,
        current_version: u16,
        reason: String,
    },
    Future {
        found_version: u16,
        current_version: u16,
    },
    Corrupt {
        current_version: u16,
        reason: String,
    },
}

impl SessionIndexEntry {
    #[must_use]
    pub fn from_event(event: &SessionEvent, offset: u64, frame_len: u64) -> Self {
        Self {
            entry_index_version: SESSION_ENTRY_INDEX_VERSION,
            sequence: event.sequence,
            offset,
            frame_len,
            kind: event_kind_tag(&event.kind).to_string(),
            schema_version: event.schema_version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionIndexHealth {
    pub session_id: SessionId,
    pub event_count: usize,
    pub last_good_offset: u64,
    pub issue_count: usize,
    pub stale: bool,
}

impl EventFileFingerprint {
    #[must_use]
    pub const fn modified_at_ms(&self) -> u64 {
        timestamp_ms(self.modified_unix_secs, self.modified_nanos)
    }

    #[must_use]
    pub const fn created_at_ms(&self) -> u64 {
        let created_at_ms = timestamp_ms(self.created_unix_secs, self.created_nanos);
        if created_at_ms == 0 {
            self.modified_at_ms()
        } else {
            created_at_ms
        }
    }
}

impl SessionIndex {
    pub fn from_report(
        session_id: SessionId,
        file: EventFileFingerprint,
        report: &SessionReadReport,
    ) -> Option<Self> {
        let mut name = None;
        let mut next_sequence = 0_u64;
        let mut has_user_message = false;
        let mut current_provider = None;
        let mut current_model = None;
        let mut current_agent = None;
        let mut latest_compaction_sequence = None;
        let mut total_metered_tokens = 0_u64;

        for event in &report.events {
            next_sequence = next_sequence.max(event.sequence.saturating_add(1));
            match &event.kind {
                SessionEventKind::SessionCreated { name: event_name }
                | SessionEventKind::SessionRenamed { name: event_name } => {
                    name.clone_from(event_name);
                }
                SessionEventKind::UserMessage { .. } => has_user_message = true,
                SessionEventKind::ModelChanged { provider, model } => {
                    current_provider = Some(provider.clone());
                    current_model = Some(model.clone());
                }
                SessionEventKind::AgentChanged { agent_id } => {
                    current_agent = Some(agent_id.clone());
                }
                SessionEventKind::ContextCompacted {
                    compacted_through_sequence,
                    ..
                } => {
                    latest_compaction_sequence = Some(*compacted_through_sequence);
                }
                SessionEventKind::ModelUsage { usage, .. } => {
                    if let Some(total) = usage.metered_total_tokens() {
                        total_metered_tokens =
                            total_metered_tokens.saturating_add(u64::from(total));
                    }
                }
                _ => {}
            }
        }

        if report.events.is_empty() {
            return None;
        }

        let created_at_ms = file.created_at_ms();
        let updated_at_ms = file.modified_at_ms();

        Some(Self {
            index_version: SESSION_INDEX_VERSION,
            session_id,
            file,
            summary: SessionSummary {
                id: session_id,
                name,
                client_count: 0,
                created_at_ms,
                updated_at_ms,
            },
            next_sequence,
            event_count: report.events.len(),
            created_at_ms,
            updated_at_ms,
            has_user_message,
            last_good_offset: report.last_good_offset,
            current_provider,
            current_model,
            current_agent,
            latest_compaction_sequence,
            total_metered_tokens,
            min_event_schema_version: report.min_schema_version,
            max_event_schema_version: report.max_schema_version,
            issues: report.issues.iter().map(SessionIndexIssue::from).collect(),
        })
    }

    pub fn from_report_metadata(
        session_id: SessionId,
        file: EventFileFingerprint,
        report: &SessionReadReport,
    ) -> Option<Self> {
        let mut index = Self::from_report(session_id, file, report)?;
        index.event_count = report.entries.len();
        Some(index)
    }

    pub(crate) fn into_state(self) -> SessionState {
        SessionState::from_index(self)
    }

    pub const fn health(&self, stale: bool) -> SessionIndexHealth {
        SessionIndexHealth {
            session_id: self.session_id,
            event_count: self.event_count,
            last_good_offset: self.last_good_offset,
            issue_count: self.issues.len(),
            stale,
        }
    }
}

impl From<&SessionReadIssue> for SessionIndexIssue {
    fn from(value: &SessionReadIssue) -> Self {
        Self {
            offset: value.offset,
            message: issue_message(&value.kind),
        }
    }
}

pub fn fingerprint(path: &Path) -> Result<EventFileFingerprint, SessionStoreError> {
    let metadata = fs::metadata(path)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok());
    let created = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(UNIX_EPOCH).ok());
    Ok(EventFileFingerprint {
        len: metadata.len(),
        modified_unix_secs: modified.as_ref().map_or(0, Duration::as_secs),
        modified_nanos: modified.map_or(0, |duration| duration.subsec_nanos()),
        created_unix_secs: created.as_ref().map_or(0, Duration::as_secs),
        created_nanos: created.map_or(0, |duration| duration.subsec_nanos()),
    })
}

pub fn index_path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("index").join(format!("{session_id}.index.json"))
}

pub fn entries_path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("index")
        .join(format!("{session_id}.entries.jsonl"))
}

pub fn load_fresh_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<Option<SessionIndex>, SessionStoreError> {
    match inspect_index(root, session_id, event_path)? {
        SessionIndexStatus::Current(index) => Ok(Some(*index)),
        SessionIndexStatus::Missing { .. }
        | SessionIndexStatus::Stale { .. }
        | SessionIndexStatus::Future { .. }
        | SessionIndexStatus::Corrupt { .. } => Ok(None),
    }
}

pub fn inspect_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<SessionIndexStatus, SessionStoreError> {
    let index_path = index_path(root, session_id);
    let contents = match fs::read_to_string(index_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionIndexStatus::Missing {
                current_version: SESSION_INDEX_VERSION,
            });
        }
        Err(error) => return Err(SessionStoreError::Io(error)),
    };
    let version = match serde_json::from_str::<RawIndexVersion>(&contents) {
        Ok(version) => version,
        Err(error) => {
            return Ok(SessionIndexStatus::Corrupt {
                current_version: SESSION_INDEX_VERSION,
                reason: error.to_string(),
            });
        }
    };
    if let Some(index_session_id) = version.session_id
        && index_session_id != session_id
    {
        return Ok(SessionIndexStatus::Stale {
            found_version: Some(version.index_version),
            current_version: SESSION_INDEX_VERSION,
            reason: format!("index session id {index_session_id} does not match {session_id}"),
        });
    }
    if version.index_version > SESSION_INDEX_VERSION {
        return Ok(SessionIndexStatus::Future {
            found_version: version.index_version,
            current_version: SESSION_INDEX_VERSION,
        });
    }
    if version.index_version != SESSION_INDEX_VERSION {
        return Ok(SessionIndexStatus::Stale {
            found_version: Some(version.index_version),
            current_version: SESSION_INDEX_VERSION,
            reason: "index version is stale".to_string(),
        });
    }
    let index = match serde_json::from_str::<SessionIndex>(&contents) {
        Ok(index) => index,
        Err(error) => {
            return Ok(SessionIndexStatus::Corrupt {
                current_version: SESSION_INDEX_VERSION,
                reason: error.to_string(),
            });
        }
    };
    let file = fingerprint(event_path)?;
    if index.session_id == session_id && index.file == file {
        Ok(SessionIndexStatus::Current(Box::new(index)))
    } else {
        Ok(SessionIndexStatus::Stale {
            found_version: Some(version.index_version),
            current_version: SESSION_INDEX_VERSION,
            reason: "event file fingerprint changed".to_string(),
        })
    }
}

pub fn write_index(root: &Path, index: &SessionIndex) -> Result<(), SessionStoreError> {
    let path = index_path(root, index.session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("index.json.tmp");
    let contents = serde_json::to_vec_pretty(index).map_err(SessionStoreError::Index)?;
    fs::write(&tmp_path, contents)?;
    fs::rename(tmp_path, path)?;
    Ok(())
}

pub fn append_entry(
    root: &Path,
    session_id: SessionId,
    entry: &SessionIndexEntry,
) -> Result<(), SessionStoreError> {
    let path = entries_path(root, session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, entry).map_err(SessionStoreError::Index)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

pub fn read_entries(
    root: &Path,
    session_id: SessionId,
) -> Result<Vec<SessionIndexEntry>, SessionStoreError> {
    let path = entries_path(root, session_id);
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry =
            serde_json::from_str::<SessionIndexEntry>(&line).map_err(SessionStoreError::Index)?;
        if entry.entry_index_version == SESSION_ENTRY_INDEX_VERSION {
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub fn write_entries(
    root: &Path,
    session_id: SessionId,
    entries: &[SessionIndexEntry],
) -> Result<(), SessionStoreError> {
    let path = entries_path(root, session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("entries.jsonl.tmp");
    let mut file = fs::File::create(&tmp_path)?;
    for entry in entries {
        serde_json::to_writer(&mut file, entry).map_err(SessionStoreError::Index)?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    fs::rename(tmp_path, path)?;
    Ok(())
}

pub fn rebuild_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<(Option<SessionIndex>, Vec<SessionEvent>), SessionStoreError> {
    let report = crate::reader::read_events(event_path)?;
    let file = fingerprint(event_path)?;
    let index = SessionIndex::from_report(session_id, file, &report);
    if let Some(index) = &index {
        write_index(root, index)?;
        write_entries(root, session_id, &report.entries)?;
    }
    Ok((index, report.events))
}

pub fn rebuild_index_metadata(
    _root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<Option<SessionIndex>, SessionStoreError> {
    let report = crate::reader::read_events(event_path)?;
    let file = fingerprint(event_path)?;
    Ok(SessionIndex::from_report_metadata(
        session_id, file, &report,
    ))
}

const fn event_kind_tag(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::SessionCreated { .. } => "session_created",
        SessionEventKind::ClientAttached { .. } => "client_attached",
        SessionEventKind::ClientDetached { .. } => "client_detached",
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantDelta { .. } => "assistant_delta",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
        SessionEventKind::ToolCallFinished { .. } => "tool_call_finished",
        SessionEventKind::PermissionRequested { .. } => "permission_requested",
        SessionEventKind::PermissionResolved { .. } => "permission_resolved",
        SessionEventKind::ModelChanged { .. } => "model_changed",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::AgentChanged { .. } => "agent_changed",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::ModelUsage { .. } => "model_usage",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
        SessionEventKind::SessionRenamed { .. } => "session_renamed",
        SessionEventKind::TraceEvent { .. } => "trace_event",
        SessionEventKind::SkillInvoked { .. } => "skill_invoked",
        SessionEventKind::SkillSuggested { .. } => "skill_suggested",
        SessionEventKind::SkillActivated { .. } => "skill_activated",
        SessionEventKind::SkillDeactivated { .. } => "skill_deactivated",
        SessionEventKind::SkillContextLoaded { .. } => "skill_context_loaded",
        SessionEventKind::SkillInvocationFailed { .. } => "skill_invocation_failed",
        SessionEventKind::AssistantReasoningDelta { .. } => "assistant_reasoning_delta",
        SessionEventKind::AssistantReasoningMessage { .. } => "assistant_reasoning_message",
    }
}

fn issue_message(kind: &SessionReadIssueKind) -> String {
    match kind {
        SessionReadIssueKind::TruncatedLength { bytes_read } => {
            format!("truncated frame length: got {bytes_read} bytes")
        }
        SessionReadIssueKind::TruncatedPayload { expected, actual } => {
            format!("truncated frame payload: expected {expected} bytes, got {actual}")
        }
        SessionReadIssueKind::OversizedFrame { frame_len } => {
            format!("frame length {frame_len} exceeds safety limit")
        }
        SessionReadIssueKind::Decode { message } => format!("decode error: {message}"),
    }
}

const fn timestamp_ms(secs: u64, nanos: u32) -> u64 {
    secs.saturating_mul(1_000)
        .saturating_add((nanos / 1_000_000) as u64)
}
