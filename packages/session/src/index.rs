use crate::reader::{SessionReadIssue, SessionReadIssueKind, SessionReadReport};
use crate::{SessionState, SessionStoreError};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId, SessionSummary};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const SESSION_INDEX_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFileFingerprint {
    pub len: u64,
    pub modified_unix_secs: u64,
    pub modified_nanos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndex {
    pub index_version: u16,
    pub session_id: SessionId,
    pub file: EventFileFingerprint,
    pub summary: SessionSummary,
    pub next_sequence: u64,
    pub event_count: usize,
    pub has_user_message: bool,
    pub last_good_offset: u64,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub current_agent: Option<String>,
    pub latest_compaction_sequence: Option<u64>,
    pub total_metered_tokens: u64,
    pub issues: Vec<SessionIndexIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexIssue {
    pub offset: u64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIndexHealth {
    pub session_id: SessionId,
    pub event_count: usize,
    pub last_good_offset: u64,
    pub issue_count: usize,
    pub stale: bool,
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

        Some(Self {
            index_version: SESSION_INDEX_VERSION,
            session_id,
            file,
            summary: SessionSummary {
                id: session_id,
                name,
                client_count: 0,
            },
            next_sequence,
            event_count: report.events.len(),
            has_user_message,
            last_good_offset: report.last_good_offset,
            current_provider,
            current_model,
            current_agent,
            latest_compaction_sequence,
            total_metered_tokens,
            issues: report.issues.iter().map(SessionIndexIssue::from).collect(),
        })
    }

    pub fn into_state(self) -> SessionState {
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
    Ok(EventFileFingerprint {
        len: metadata.len(),
        modified_unix_secs: modified.as_ref().map_or(0, std::time::Duration::as_secs),
        modified_nanos: modified.map_or(0, |duration| duration.subsec_nanos()),
    })
}

pub fn index_path(root: &Path, session_id: SessionId) -> PathBuf {
    root.join("index").join(format!("{session_id}.index.json"))
}

pub fn load_fresh_index(
    root: &Path,
    session_id: SessionId,
    event_path: &Path,
) -> Result<Option<SessionIndex>, SessionStoreError> {
    let index_path = index_path(root, session_id);
    let Ok(contents) = fs::read_to_string(index_path) else {
        return Ok(None);
    };
    let Ok(index) = serde_json::from_str::<SessionIndex>(&contents) else {
        return Ok(None);
    };
    let file = fingerprint(event_path)?;
    if index.index_version == SESSION_INDEX_VERSION
        && index.session_id == session_id
        && index.file == file
    {
        Ok(Some(index))
    } else {
        Ok(None)
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
    }
    Ok((index, report.events))
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
