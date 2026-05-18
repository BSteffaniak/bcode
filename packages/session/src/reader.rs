use crate::SessionStoreError;
use bcode_session_models::SessionEvent;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;

const FRAME_LEN_BYTES: usize = 4;
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
    pub last_good_offset: u64,
    pub issues: Vec<SessionReadIssue>,
}

pub fn read_events(path: &Path) -> Result<SessionReadReport, SessionStoreError> {
    let mut file = File::open(path)?;
    let mut events = Vec::new();
    let mut issues = Vec::new();
    let mut offset = 0_u64;
    let mut last_good_offset = 0_u64;

    loop {
        let frame_offset = offset;
        let Some(payload_len) = read_frame_len(&mut file, &mut offset, frame_offset, &mut issues)?
        else {
            break;
        };
        if payload_len > MAX_SESSION_EVENT_FRAME_BYTES {
            issues.push(SessionReadIssue {
                offset: frame_offset,
                kind: SessionReadIssueKind::OversizedFrame {
                    frame_len: payload_len,
                },
            });
            break;
        }
        let Some(payload) = read_frame_payload(
            &mut file,
            payload_len,
            &mut offset,
            frame_offset,
            &mut issues,
        )?
        else {
            break;
        };
        match bmux_codec::from_bytes(&payload) {
            Ok(event) => events.push(event),
            Err(error) => issues.push(SessionReadIssue {
                offset: frame_offset,
                kind: SessionReadIssueKind::Decode {
                    message: error.to_string(),
                },
            }),
        }
        last_good_offset = offset;
    }

    Ok(SessionReadReport {
        events,
        last_good_offset,
        issues,
    })
}

fn read_frame_len(
    file: &mut File,
    offset: &mut u64,
    frame_offset: u64,
    issues: &mut Vec<SessionReadIssue>,
) -> Result<Option<usize>, SessionStoreError> {
    let mut len_bytes = [0_u8; FRAME_LEN_BYTES];
    let mut bytes_read = 0_usize;
    while bytes_read < FRAME_LEN_BYTES {
        let read = file.read(&mut len_bytes[bytes_read..])?;
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
    Ok(Some(u32::from_le_bytes(len_bytes) as usize))
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
