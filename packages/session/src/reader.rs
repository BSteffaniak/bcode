use crate::SessionStoreError;
use crate::index::SessionIndexEntry;
use bcode_session_models::{
    ClientId, ModelTurnOutcome, SessionEvent, SessionEventKind, SessionId, SessionTokenUsage,
    SessionTraceEvent,
};
use bcode_skill_models::{SkillActivationMode, SkillId, SkillSource};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

const FRAME_LEN_BYTES: usize = 4;
const FRAME_V2_MAGIC: &[u8; 4] = b"BSE2";
const FRAME_V2_VERSION: u16 = 2;
const FRAME_V2_HEADER_REST_BYTES: usize = 40;
const FRAME_V2_HEADER_BYTES: usize = FRAME_LEN_BYTES + FRAME_V2_HEADER_REST_BYTES;
const MAX_SESSION_EVENT_FRAME_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReadIssueKind {
    TruncatedLength { bytes_read: usize },
    TruncatedPayload { expected: usize, actual: usize },
    OversizedFrame { frame_len: usize },
    Decode { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReadIssue {
    pub offset: u64,
    pub kind: SessionReadIssueKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReadReport {
    pub events: Vec<SessionEvent>,
    pub entries: Vec<SessionIndexEntry>,
    pub last_good_offset: u64,
    pub issues: Vec<SessionReadIssue>,
    pub min_schema_version: Option<u16>,
    pub max_schema_version: Option<u16>,
}

pub fn read_events(path: &Path) -> Result<SessionReadReport, SessionStoreError> {
    let mut file = File::open(path)?;
    let mut events = Vec::new();
    let mut entries = Vec::new();
    let mut issues = Vec::new();
    let mut offset = 0_u64;
    let mut last_good_offset = 0_u64;

    loop {
        let frame_offset = offset;
        let Some(payload) =
            read_next_frame_payload(&mut file, &mut offset, frame_offset, &mut issues)?
        else {
            break;
        };
        match decode_session_event(&payload) {
            Ok(event) => {
                let frame_len = offset.saturating_sub(frame_offset);
                entries.push(SessionIndexEntry::from_event(
                    &event,
                    frame_offset,
                    frame_len,
                ));
                events.push(event);
            }
            Err(error) => issues.push(SessionReadIssue {
                offset: frame_offset,
                kind: SessionReadIssueKind::Decode {
                    message: error.to_string(),
                },
            }),
        }
        last_good_offset = offset;
    }

    let min_schema_version = events.iter().map(|event| event.schema_version).min();
    let max_schema_version = events.iter().map(|event| event.schema_version).max();
    Ok(SessionReadReport {
        events,
        entries,
        last_good_offset,
        issues,
        min_schema_version,
        max_schema_version,
    })
}

pub fn read_event_at(path: &Path, offset: u64) -> Result<SessionEvent, SessionStoreError> {
    let mut events = read_events_at_offsets(path, &[offset])?;
    events
        .pop()
        .ok_or_else(|| SessionStoreError::InvalidSessionId(format!("no event at offset {offset}")))
}

pub fn read_events_at_offsets(
    path: &Path,
    offsets: &[u64],
) -> Result<Vec<SessionEvent>, SessionStoreError> {
    let mut file = File::open(path)?;
    offsets
        .iter()
        .map(|offset| {
            file.seek(SeekFrom::Start(*offset))?;
            let payload = read_frame_payload_at_current_offset(&mut file)?;
            decode_session_event(&payload).map_err(SessionStoreError::Decode)
        })
        .collect()
}

fn decode_session_event(payload: &[u8]) -> Result<SessionEvent, bmux_codec::Error> {
    match bmux_codec::from_bytes(payload) {
        Ok(event) => Ok(event),
        Err(primary) => bmux_codec::from_bytes::<BadReasoningOrderSessionEvent>(payload)
            .map(Into::into)
            .map_err(|_| primary),
    }
}

#[derive(Debug, Deserialize)]
struct BadReasoningOrderSessionEvent {
    schema_version: u16,
    sequence: u64,
    session_id: SessionId,
    kind: BadReasoningOrderSessionEventKind,
}

impl From<BadReasoningOrderSessionEvent> for SessionEvent {
    fn from(value: BadReasoningOrderSessionEvent) -> Self {
        Self {
            schema_version: value.schema_version,
            sequence: value.sequence,
            session_id: value.session_id,
            provenance: None,
            kind: value.kind.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BadReasoningOrderSessionEventKind {
    SessionCreated {
        name: Option<String>,
        working_directory: std::path::PathBuf,
    },
    ClientAttached {
        client_id: ClientId,
    },
    ClientDetached {
        client_id: ClientId,
    },
    UserMessage {
        client_id: ClientId,
        text: String,
    },
    AssistantReasoningDelta {
        text: String,
    },
    AssistantReasoningMessage {
        text: String,
    },
    AssistantDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallRequested {
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    ToolCallFinished {
        tool_call_id: String,
        result: String,
        #[serde(default)]
        is_error: bool,
    },
    PermissionRequested {
        permission_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    PermissionResolved {
        permission_id: String,
        approved: bool,
    },
    ModelChanged {
        provider: String,
        model: String,
    },
    SystemMessage {
        text: String,
    },
    AgentChanged {
        agent_id: String,
    },
    ModelTurnStarted {
        turn_id: String,
    },
    ModelTurnFinished {
        turn_id: String,
        outcome: ModelTurnOutcome,
        #[serde(default)]
        message: Option<String>,
    },
    ModelUsage {
        turn_id: String,
        usage: SessionTokenUsage,
    },
    ContextCompacted {
        summary: String,
        compacted_through_sequence: u64,
    },
    SessionRenamed {
        name: Option<String>,
    },
    TraceEvent {
        trace: Box<SessionTraceEvent>,
    },
    SkillInvoked {
        skill_id: SkillId,
        arguments: String,
        #[serde(default)]
        source: Option<SkillSource>,
        invoked_at_ms: u64,
    },
    SkillSuggested {
        skill_id: SkillId,
        #[serde(default)]
        reason: Option<String>,
        suggested_at_ms: u64,
    },
    SkillActivated {
        skill_id: SkillId,
        #[serde(default)]
        source: Option<SkillSource>,
        mode: SkillActivationMode,
        activated_at_ms: u64,
    },
    SkillDeactivated {
        skill_id: SkillId,
        deactivated_at_ms: u64,
    },
    SkillContextLoaded {
        skill_id: SkillId,
        bytes_loaded: usize,
        truncated: bool,
        loaded_at_ms: u64,
    },
    SkillInvocationFailed {
        skill_id: SkillId,
        error: String,
        failed_at_ms: u64,
    },
}

impl From<BadReasoningOrderSessionEventKind> for SessionEventKind {
    // This compatibility shim must enumerate every persisted variant in the bad order.
    #[allow(clippy::too_many_lines)]
    fn from(value: BadReasoningOrderSessionEventKind) -> Self {
        match value {
            BadReasoningOrderSessionEventKind::SessionCreated {
                name,
                working_directory,
            } => Self::SessionCreated {
                name,
                working_directory,
            },
            BadReasoningOrderSessionEventKind::ClientAttached { client_id } => {
                Self::ClientAttached { client_id }
            }
            BadReasoningOrderSessionEventKind::ClientDetached { client_id } => {
                Self::ClientDetached { client_id }
            }
            BadReasoningOrderSessionEventKind::UserMessage { client_id, text } => {
                Self::UserMessage { client_id, text }
            }
            BadReasoningOrderSessionEventKind::AssistantReasoningDelta { text } => {
                Self::AssistantReasoningDelta { text }
            }
            BadReasoningOrderSessionEventKind::AssistantReasoningMessage { text } => {
                Self::AssistantReasoningMessage { text }
            }
            BadReasoningOrderSessionEventKind::AssistantDelta { text } => {
                Self::AssistantDelta { text }
            }
            BadReasoningOrderSessionEventKind::AssistantMessage { text } => {
                Self::AssistantMessage { text }
            }
            BadReasoningOrderSessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            } => Self::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
            },
            BadReasoningOrderSessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
            } => Self::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output: None,
            },
            BadReasoningOrderSessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            } => Self::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            },
            BadReasoningOrderSessionEventKind::PermissionResolved {
                permission_id,
                approved,
            } => Self::PermissionResolved {
                permission_id,
                approved,
            },
            BadReasoningOrderSessionEventKind::ModelChanged { provider, model } => {
                Self::ModelChanged { provider, model }
            }
            BadReasoningOrderSessionEventKind::SystemMessage { text } => {
                Self::SystemMessage { text }
            }
            BadReasoningOrderSessionEventKind::AgentChanged { agent_id } => {
                Self::AgentChanged { agent_id }
            }
            BadReasoningOrderSessionEventKind::ModelTurnStarted { turn_id } => {
                Self::ModelTurnStarted { turn_id }
            }
            BadReasoningOrderSessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } => Self::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            },
            BadReasoningOrderSessionEventKind::ModelUsage { turn_id, usage } => {
                Self::ModelUsage { turn_id, usage }
            }
            BadReasoningOrderSessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            } => Self::ContextCompacted {
                summary,
                compacted_through_sequence,
            },
            BadReasoningOrderSessionEventKind::SessionRenamed { name } => {
                Self::SessionRenamed { name }
            }
            BadReasoningOrderSessionEventKind::TraceEvent { trace } => Self::TraceEvent { trace },
            BadReasoningOrderSessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            } => Self::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            },
            BadReasoningOrderSessionEventKind::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            } => Self::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            },
            BadReasoningOrderSessionEventKind::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            } => Self::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            },
            BadReasoningOrderSessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            } => Self::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            },
            BadReasoningOrderSessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
            } => Self::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
            },
            BadReasoningOrderSessionEventKind::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            } => Self::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            },
        }
    }
}

fn read_next_frame_payload(
    file: &mut File,
    offset: &mut u64,
    frame_offset: u64,
    issues: &mut Vec<SessionReadIssue>,
) -> Result<Option<Vec<u8>>, SessionStoreError> {
    let mut first_bytes = [0_u8; FRAME_LEN_BYTES];
    let mut bytes_read = 0_usize;
    while bytes_read < FRAME_LEN_BYTES {
        let read = file.read(&mut first_bytes[bytes_read..])?;
        if read == 0 {
            if bytes_read > 0 {
                issues.push(SessionReadIssue {
                    offset: frame_offset,
                    kind: SessionReadIssueKind::TruncatedLength { bytes_read },
                });
            }
            return Ok(None);
        }
        bytes_read += read;
        *offset = offset.saturating_add(read.try_into().unwrap_or(u64::MAX));
    }

    if &first_bytes == FRAME_V2_MAGIC {
        return read_v2_frame_payload(file, offset, frame_offset, issues);
    }

    let payload_len = u32::from_le_bytes(first_bytes) as usize;
    if payload_len > MAX_SESSION_EVENT_FRAME_BYTES {
        issues.push(SessionReadIssue {
            offset: frame_offset,
            kind: SessionReadIssueKind::OversizedFrame {
                frame_len: payload_len,
            },
        });
        return Ok(None);
    }
    read_frame_payload(file, payload_len, offset, frame_offset, issues)
}

fn read_frame_payload_at_current_offset(file: &mut File) -> Result<Vec<u8>, SessionStoreError> {
    let mut first_bytes = [0_u8; FRAME_LEN_BYTES];
    file.read_exact(&mut first_bytes)?;
    if &first_bytes == FRAME_V2_MAGIC {
        let mut rest = [0_u8; FRAME_V2_HEADER_REST_BYTES];
        file.read_exact(&mut rest)?;
        let frame_version = u16::from_le_bytes([rest[0], rest[1]]);
        let payload_len = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        if frame_version != FRAME_V2_VERSION {
            return Err(SessionStoreError::UnsupportedFrameVersion(frame_version));
        }
        if payload_len > MAX_SESSION_EVENT_FRAME_BYTES {
            return Err(SessionStoreError::FrameTooLarge(payload_len));
        }
        let mut payload = vec![0_u8; payload_len];
        file.read_exact(&mut payload)?;
        let checksum = Sha256::digest(&payload);
        if checksum.as_slice() != &rest[8..40] {
            return Err(SessionStoreError::ChecksumMismatch);
        }
        return Ok(payload);
    }

    let payload_len = u32::from_le_bytes(first_bytes) as usize;
    if payload_len > MAX_SESSION_EVENT_FRAME_BYTES {
        return Err(SessionStoreError::FrameTooLarge(payload_len));
    }
    let mut payload = vec![0_u8; payload_len];
    file.read_exact(&mut payload)?;
    Ok(payload)
}

fn read_v2_frame_payload(
    file: &mut File,
    offset: &mut u64,
    frame_offset: u64,
    issues: &mut Vec<SessionReadIssue>,
) -> Result<Option<Vec<u8>>, SessionStoreError> {
    let mut rest = [0_u8; FRAME_V2_HEADER_REST_BYTES];
    let mut bytes_read = 0_usize;
    while bytes_read < FRAME_V2_HEADER_REST_BYTES {
        let read = file.read(&mut rest[bytes_read..])?;
        if read == 0 {
            issues.push(SessionReadIssue {
                offset: frame_offset,
                kind: SessionReadIssueKind::TruncatedPayload {
                    expected: FRAME_V2_HEADER_BYTES,
                    actual: FRAME_LEN_BYTES + bytes_read,
                },
            });
            return Ok(None);
        }
        bytes_read += read;
        *offset = offset.saturating_add(read.try_into().unwrap_or(u64::MAX));
    }

    let frame_version = u16::from_le_bytes([rest[0], rest[1]]);
    if frame_version != FRAME_V2_VERSION {
        issues.push(SessionReadIssue {
            offset: frame_offset,
            kind: SessionReadIssueKind::Decode {
                message: format!("unsupported session frame version {frame_version}"),
            },
        });
        return Ok(None);
    }
    let payload_len = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
    if payload_len > MAX_SESSION_EVENT_FRAME_BYTES {
        issues.push(SessionReadIssue {
            offset: frame_offset,
            kind: SessionReadIssueKind::OversizedFrame {
                frame_len: payload_len,
            },
        });
        return Ok(None);
    }
    let Some(payload) = read_frame_payload(file, payload_len, offset, frame_offset, issues)? else {
        return Ok(None);
    };
    let checksum = Sha256::digest(&payload);
    if checksum.as_slice() != &rest[8..40] {
        issues.push(SessionReadIssue {
            offset: frame_offset,
            kind: SessionReadIssueKind::Decode {
                message: "session event frame checksum mismatch".to_string(),
            },
        });
        return Ok(None);
    }
    Ok(Some(payload))
}

fn read_frame_payload(
    file: &mut File,
    payload_len: usize,
    offset: &mut u64,
    frame_offset: u64,
    issues: &mut Vec<SessionReadIssue>,
) -> Result<Option<Vec<u8>>, SessionStoreError> {
    let mut payload = vec![0_u8; payload_len];
    let mut bytes_read = 0_usize;
    while bytes_read < payload_len {
        let read = file.read(&mut payload[bytes_read..])?;
        if read == 0 {
            issues.push(SessionReadIssue {
                offset: frame_offset,
                kind: SessionReadIssueKind::TruncatedPayload {
                    expected: payload_len,
                    actual: bytes_read,
                },
            });
            return Ok(None);
        }
        bytes_read += read;
        *offset = offset.saturating_add(read.try_into().unwrap_or(u64::MAX));
    }
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::io::Write as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Serialize)]
    struct BadOrderTestSessionEvent {
        schema_version: u16,
        sequence: u64,
        session_id: SessionId,
        kind: BadOrderTestSessionEventKind,
    }

    #[allow(dead_code)]
    #[derive(Serialize)]
    enum BadOrderTestSessionEventKind {
        SessionCreated {
            name: Option<String>,
            working_directory: std::path::PathBuf,
        },
        ClientAttached {
            client_id: ClientId,
        },
        ClientDetached {
            client_id: ClientId,
        },
        UserMessage {
            client_id: ClientId,
            text: String,
        },
        AssistantReasoningDelta {
            text: String,
        },
        AssistantReasoningMessage {
            text: String,
        },
        AssistantDelta {
            text: String,
        },
    }

    #[test]
    fn reads_events_written_with_bad_reasoning_variant_order() {
        let session_id = SessionId::new();
        let path = temp_event_path();
        let bad_order_event = BadOrderTestSessionEvent {
            schema_version: 9,
            sequence: 0,
            session_id,
            kind: BadOrderTestSessionEventKind::AssistantDelta {
                text: "hello".to_string(),
            },
        };
        let payload = bmux_codec::to_vec(&bad_order_event).expect("event should encode");
        {
            let mut file = std::fs::File::create(&path).expect("event log should create");
            file.write_all(
                &u32::try_from(payload.len())
                    .expect("payload should fit")
                    .to_le_bytes(),
            )
            .expect("length should write");
            file.write_all(&payload).expect("payload should write");
        }

        let report = read_events(&path).expect("bad order event should read");
        assert!(report.issues.is_empty());
        assert_eq!(report.events.len(), 1);
        assert!(matches!(
            &report.events[0].kind,
            SessionEventKind::AssistantDelta { text } if text == "hello"
        ));

        std::fs::remove_file(path).expect("event log should remove");
    }

    fn temp_event_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bcode-session-reader-test-{}-{nanos}.events",
            std::process::id()
        ))
    }
}
