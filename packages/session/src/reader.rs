use crate::SessionStoreError;
use crate::index::SessionIndexEntry;
use bcode_session_models::SessionEvent;
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
        match bmux_codec::from_bytes::<SessionEvent>(&payload) {
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
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let payload = read_frame_payload_at_current_offset(&mut file)?;
    bmux_codec::from_bytes(&payload).map_err(SessionStoreError::Decode)
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
