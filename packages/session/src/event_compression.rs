//! Lossless current-format event payload envelope.
//!
//! This codec is independent of the event schema. It is not enabled for canonical writes until a
//! storage-epoch migration fences incompatible readers. JSON remains the logical source content;
//! decoding preserves every byte, including private evidence and unknown JSON fields.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use std::borrow::Cow;
use std::io::{self, Read as _};

const PREFIX: &str = "bcode-event-zstd:";
const VERSION: &str = "1";
/// Maximum expanded payload accepted by this envelope, independent of untrusted advertised sizes.
pub const MAX_COMPRESSED_EVENT_BYTES: usize = 16 * 1024 * 1024;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid compressed session event payload",
    )
}

/// Encode exact logical JSON bytes, returning the original if compression has no net saving.
///
/// # Errors
/// Rejects oversized or non-JSON input, invalid compression level, and codec failure.
pub fn compress_event_payload(payload: &str, level: i32) -> io::Result<String> {
    if payload.len() > MAX_COMPRESSED_EVENT_BYTES || !(1..=22).contains(&level) {
        return Err(invalid());
    }
    serde_json::from_str::<serde_json::Value>(payload).map_err(|_| invalid())?;
    let compressed = zstd::bulk::compress(payload.as_bytes(), level)?;
    let encoded = format!(
        "{PREFIX}{VERSION}:{}:{:x}:{}",
        payload.len(),
        Sha256::digest(payload.as_bytes()),
        base64::engine::general_purpose::STANDARD.encode(compressed)
    );
    Ok(if encoded.len() < payload.len() {
        encoded
    } else {
        payload.to_owned()
    })
}

/// Recover logical payload bytes without interpreting historical event variants.
///
/// Uncompressed JSON is borrowed, preserving ordinary read behavior. Reserved envelope prefixes
/// reject unsupported versions rather than falling back to raw JSON. Memory/window/expansion limits
/// are validated before allocations and decompression; checksum and UTF-8 must match exactly.
///
/// # Errors
/// Returns an error for malformed/future envelopes, oversized content, checksum, UTF-8, or codec
/// failures. Never repairs or substitutes an older interpretation.
pub fn decode_event_payload(payload: &str) -> io::Result<Cow<'_, str>> {
    let Some(envelope) = payload.strip_prefix(PREFIX) else {
        return Ok(Cow::Borrowed(payload));
    };
    let mut fields = envelope.splitn(4, ':');
    if fields.next() != Some(VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported compressed event payload version",
        ));
    }
    let length = fields
        .next()
        .ok_or_else(invalid)?
        .parse::<usize>()
        .map_err(|_| invalid())?;
    let checksum = fields.next().ok_or_else(invalid)?;
    let encoded = fields.next().ok_or_else(invalid)?;
    if length > MAX_COMPRESSED_EVENT_BYTES
        || checksum.len() != 64
        || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
        || encoded.len() > (MAX_COMPRESSED_EVENT_BYTES + 1024) * 4 / 3 + 4
    {
        return Err(invalid());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid())?;
    let mut decoder = zstd::stream::read::Decoder::new(bytes.as_slice())?;
    decoder.window_log_max(24)?;
    let mut expanded = Vec::with_capacity(length);
    decoder.take(length as u64 + 1).read_to_end(&mut expanded)?;
    if expanded.len() != length
        || !format!("{:x}", Sha256::digest(&expanded)).eq_ignore_ascii_case(checksum)
    {
        return Err(invalid());
    }
    String::from_utf8(expanded)
        .map(Cow::Owned)
        .map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_exact_json_and_private_fields_at_both_levels() {
        let original = format!(
            "{{\n  \"unknown\": \"{}\", \"original_usage\": {{\"private\":true}}\n}}",
            "世界 terminal output ".repeat(10_000)
        );
        for level in [1, 12] {
            let compressed = compress_event_payload(&original, level).expect("encode");
            assert!(compressed.len() < original.len());
            assert_eq!(decode_event_payload(&compressed).expect("decode"), original);
        }
        let tiny = "{\"x\":1}";
        assert_eq!(compress_event_payload(tiny, 1).expect("tiny"), tiny);
        assert!(matches!(
            decode_event_payload(tiny).expect("borrow"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn corrupt_future_and_oversized_envelopes_fail_closed() {
        let original = serde_json::json!({"text": "repeat".repeat(10_000)}).to_string();
        let compressed = compress_event_payload(&original, 1).expect("encode");
        assert!(decode_event_payload(&compressed.replace("zstd:1:", "zstd:2:")).is_err());
        let mut corrupt = compressed.clone();
        corrupt.pop();
        assert!(decode_event_payload(&corrupt).is_err());
        assert!(
            decode_event_payload(&format!(
                "{PREFIX}1:{}:{}:AAAA",
                MAX_COMPRESSED_EVENT_BYTES + 1,
                "0".repeat(64)
            ))
            .is_err()
        );
        assert!(
            decode_event_payload(&format!(
                "{PREFIX}1:1:{}:{}",
                "0".repeat(64),
                compressed.rsplit(':').next().expect("body")
            ))
            .is_err()
        );
        assert!(compress_event_payload("invalid", 1).is_err());
        assert!(compress_event_payload("{}", 23).is_err());
    }
}
