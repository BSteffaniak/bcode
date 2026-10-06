//! Portable, explicitly requested access to retained session trace payloads.

use crate::{SessionId, TraceBlobCompleteness, TraceRedaction};
use serde::{Deserialize, Serialize};

/// Independently evolving trace export contract version.
pub const SESSION_TRACE_EXPORT_VERSION: u16 = 1;
/// Maximum retained blob verified by one operation (one MiB).
pub const MAX_SESSION_TRACE_BLOB_BYTES: u32 = 1024 * 1024;

/// Read a range of the trace blob referenced by one canonical session event.
///
/// No filesystem path or caller-authored blob reference is accepted. Unknown versions
/// fail closed. Access is sensitive diagnostic access, not ordinary frontend history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadSessionTraceBlobRequest {
    /// Must equal [`SESSION_TRACE_EXPORT_VERSION`].
    pub version: u16,
    /// Canonical owning session in the selected state location.
    pub session_id: SessionId,
    /// Exact canonical sequence of a trace event retaining a blob reference.
    pub sequence: u64,
    /// Byte offset in retained content; EOF is permitted.
    pub offset: u64,
    /// Positive maximum bytes returned, at most [`MAX_SESSION_TRACE_BLOB_BYTES`].
    pub length: u32,
}

/// Verified retained trace bytes, without exposing the store's private relative path.
///
/// Hash and byte count describe retained content, not necessarily the original input.
/// `eof` refers only to retained bytes; it does not imply capture completeness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTraceBlobRange {
    /// Response contract version.
    pub version: u16,
    /// Owning session.
    pub session_id: SessionId,
    /// Canonical referring event sequence.
    pub sequence: u64,
    /// SHA-256 of the entire verified retained blob, lowercase hex.
    pub sha256: String,
    /// Validated retained media type.
    pub content_type: String,
    /// Verified total retained byte count.
    pub byte_len: u64,
    /// Capture's persisted redaction status; automatic redaction is not a secret-free guarantee.
    pub redaction: TraceRedaction,
    /// Capture's persisted completeness, unchanged by successful verification.
    pub completeness: TraceBlobCompleteness,
    /// Start of this range.
    pub offset: u64,
    /// Raw bytes, not UTF-8 reconstructed or repaired content.
    pub bytes: Vec<u8>,
    /// Whether this range reaches retained EOF.
    pub eof: bool,
}
