//! Independently negotiated payload extension; no paths or incoming origin authority.
use bcode_xray::{XrayError, XrayPayloadRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Payload extension version, independent of event and service ABIs.
pub const PAYLOAD_VERSION: u16 = 1;
/// Maximum retained bytes per transfer.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// JSON byte arrays are deliberately small enough for the bridge request bound.
pub const MAX_CHUNK_BYTES: usize = 4096;
/// Maximum simultaneous transfers in one host lease.
pub const MAX_TRANSFERS: usize = 4;

/// Host-bound payload operations. Transfer IDs are opaque, lease-local host allocations.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PayloadControl {
    Negotiate {
        version: u16,
    },
    Begin {
        byte_len: usize,
        sha256: String,
        media_type: String,
    },
    Chunk {
        transfer: u64,
        offset: usize,
        bytes: Vec<u8>,
    },
    Finish {
        transfer: u64,
    },
    Abort {
        transfer: u64,
    },
}

/// Explicit payload acknowledgement; absence means unsupported, never empty success.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum PayloadReply {
    Supported {
        version: u16,
        max_bytes: usize,
        chunk_bytes: usize,
    },
    Accepted {
        transfer: u64,
    },
    Progress,
    Retained {
        reference: XrayPayloadRef,
    },
    Unavailable,
}

pub(super) fn capture(
    sink: &super::CallbackSink,
    media_type: &str,
    bytes: &[u8],
) -> Result<Option<XrayPayloadRef>, XrayError> {
    if !super::XrayCaptureSink::deep_capture_active(sink) {
        return Ok(None);
    }
    if bytes.len() > MAX_PAYLOAD_BYTES || media_type.len() > 128 {
        return Err(XrayError::PayloadTooLarge);
    }
    let request = |operation| {
        sink.control(super::DiagnosticControlOperation::Payload(operation))?
            .payload
    };
    let Some(PayloadReply::Supported {
        version: PAYLOAD_VERSION,
        max_bytes,
        chunk_bytes,
    }) = request(PayloadControl::Negotiate {
        version: PAYLOAD_VERSION,
    })
    else {
        return Err(XrayError::PayloadCaptureUnsupported);
    };
    if bytes.len() > max_bytes || chunk_bytes == 0 || chunk_bytes > MAX_CHUNK_BYTES {
        return Err(XrayError::PayloadTooLarge);
    }
    let digest = hex::encode(Sha256::digest(bytes));
    let Some(PayloadReply::Accepted { transfer }) = request(PayloadControl::Begin {
        byte_len: bytes.len(),
        sha256: digest.clone(),
        media_type: media_type.into(),
    }) else {
        return Err(XrayError::PayloadCaptureUnsupported);
    };
    // Every exit after Begin must release the host reservation, including an unwinding
    // caller. Finish consumes it on success; Abort is deliberately idempotent.
    let mut pending = PendingTransfer {
        sink,
        transfer: Some(transfer),
    };
    for (index, chunk) in bytes.chunks(chunk_bytes).enumerate() {
        if !matches!(
            request(PayloadControl::Chunk {
                transfer,
                offset: index * chunk_bytes,
                bytes: chunk.to_vec(),
            }),
            Some(PayloadReply::Progress)
        ) {
            return Err(XrayError::PayloadCaptureUnsupported);
        }
    }
    let result = request(PayloadControl::Finish { transfer });
    match result {
        Some(PayloadReply::Retained { reference })
            if reference.schema_version == bcode_xray::XRAY_SCHEMA_VERSION
                && !reference.payload_id.is_empty()
                && reference.session_id == sink.session_id
                && reference.content_type == media_type
                && reference.byte_len == bytes.len() as u64
                && reference.sha256 == digest =>
        {
            pending.transfer = None;
            Ok(Some(reference))
        }
        _ => Err(XrayError::PayloadCaptureUnsupported),
    }
}

struct PendingTransfer<'a> {
    sink: &'a super::CallbackSink,
    transfer: Option<u64>,
}

impl Drop for PendingTransfer<'_> {
    fn drop(&mut self) {
        if let Some(transfer) = self.transfer {
            let _ = self
                .sink
                .control(super::DiagnosticControlOperation::Payload(
                    PayloadControl::Abort { transfer },
                ));
        }
    }
}
