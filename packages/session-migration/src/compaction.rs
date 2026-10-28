//! Frozen decoding and source reconstruction for the retired character-batched compactor.
//! This module converts data only; it never dispatches or resumes historical execution.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Durable provenance for accepted source converted to the capacity-first engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConversion {
    /// Exact old checkpoint sequence; the publisher must compare-and-supersede it.
    pub checkpoint_sequence: u64,
    /// Digest of its complete serialized payload.
    pub checkpoint_sha256: String,
    /// Normalized conversion source; caller must not mark it covered merely by reading it.
    pub source: String,
    /// Serialized attributed source bytes already accepted by the old operation.
    pub accepted_bytes: usize,
    /// All old reservations, including rejected or uncertain calls; never refunded.
    pub reserved_requests: u32,
}

/// Whether a decoded checkpoint belongs to the retired fixed-character writer.
/// This classification never authorizes dispatch or conversion by itself.
#[must_use]
pub fn is_character_compaction(packing: &str) -> bool {
    packing.starts_with("legacy-chars-v1")
}

/// Convert one bounded historical manifest. The frozen field ordering is part of its digest.
#[derive(Serialize)]
struct Fragment<'a> {
    sequence: Option<u64>,
    provenance: &'a str,
    byte_offset: usize,
    #[serde(rename = "final_fragment")]
    final_piece: bool,
    text: &'a str,
}

/// Reconstruct the retired v1 fragment manifest from canonical source, validating every digest.
///
/// # Errors
/// Rejects unknown packing, malformed manifests/cursors, source changes, or oversized input.
#[allow(clippy::too_many_lines)] // One bounded frozen source reconstruction and verification.
pub fn convert_character_compaction(
    payload: &str,
    checkpoint_sequence: u64,
    previous: Option<&str>,
    records: &[String],
) -> Result<CompactionConversion, &'static str> {
    if payload.len() > 262_144 || records.iter().map(String::len).sum::<usize>() > 512_000 {
        return Err("historical compaction bounds exceeded");
    }
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|_| "invalid historical compaction payload")?;
    let policy = value["packing_policy"]
        .as_str()
        .unwrap_or("legacy-chars-v1");
    if value["format"] != "bcode.local-compaction.v1" || !policy.starts_with("legacy-chars-v1") {
        return Err("unsupported historical compaction format");
    }
    let manifest = value["manifest"]
        .as_array()
        .ok_or("missing historical manifest")?;
    let cursor = value["next_chunk"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or("invalid historical cursor")?;
    if cursor > manifest.len() || manifest.len() > 32 || payload.len() > 262_144 {
        return Err("historical compaction bounds exceeded");
    }
    let mut chunks = Vec::<String>::new();
    for (text, prior) in previous
        .into_iter()
        .map(|text| (text, true))
        .chain(records.iter().map(|text| (text.as_str(), false)))
    {
        let (sequence, provenance) = if prior {
            (None, "prior_derived_summary")
        } else {
            text.split_once(":\n")
                .and_then(|(header, _)| header.strip_prefix('#'))
                .and_then(|header| header.split_once(' '))
                .map_or((None, "derived"), |(sequence, role)| {
                    (
                        sequence.parse::<u64>().ok(),
                        match role {
                            "user" => "user",
                            "assistant" => "assistant",
                            "system" => "historical_system",
                            _ => "untrusted_tool_or_derived",
                        },
                    )
                })
        };
        let mut offset: usize = 0;
        loop {
            let mut end = offset.saturating_add(2000).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let encoded = serde_json::to_string(&Fragment {
                sequence,
                provenance,
                byte_offset: offset,
                final_piece: end == text.len(),
                text: &text[offset..end],
            })
            .map_err(|_| "cannot encode historical fragment")?;
            if chunks
                .last()
                .is_none_or(|chunk| chunk.len() + encoded.len() + 1 > 16_000)
            {
                chunks.push(String::new());
            }
            if chunks.len() > 32 {
                return Err("historical source exceeds manifest");
            }
            chunks
                .last_mut()
                .ok_or("missing chunk")?
                .push_str(&(encoded + "\n"));
            if end == text.len() {
                break;
            }
            offset = end;
        }
    }
    if chunks.len() != manifest.len()
        || chunks.iter().zip(manifest).any(|(chunk, expected)| {
            serde_json::to_vec(chunk).map_or(true, |bytes| {
                expected.as_str() != Some(hex::encode(Sha256::digest(bytes)).as_str())
            })
        })
    {
        return Err("historical source manifest changed");
    }
    let reserved_requests = ["requests", "consolidation_requests"]
        .into_iter()
        .try_fold(0_u32, |total, key| {
            value[key].as_array().map_or(Ok(total), |counts| {
                counts.iter().try_fold(total, |total, count| {
                    let count = count
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("invalid reservation")?;
                    total.checked_add(count).ok_or("reservation overflow")
                })
            })
        })?
        .checked_add(
            value["contract_continuation"]["requests"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
        )
        .ok_or("reservation overflow")?;
    Ok(CompactionConversion {
        checkpoint_sequence,
        checkpoint_sha256: hex::encode(Sha256::digest(payload.as_bytes())),
        accepted_bytes: chunks[..cursor].iter().map(String::len).sum(),
        source: chunks.concat(),
        reserved_requests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstructs_unicode_source_and_preserves_spent_requests() {
        let record = "#7 user:\n只读🙂".to_owned();
        let encoded = "{\"sequence\":7,\"provenance\":\"user\",\"byte_offset\":0,\"final_fragment\":true,\"text\":\"#7 user:\\n只读🙂\"}\n";
        let hash = hex::encode(Sha256::digest(serde_json::to_vec(encoded).unwrap()));
        let payload = serde_json::json!({
            "format":"bcode.local-compaction.v1", "packing_policy":"legacy-chars-v1", "manifest":[hash],
            "next_chunk":1, "requests":[2], "consolidation_requests":[1], "contract_continuation":{"requests":1}
        }).to_string();
        let converted =
            convert_character_compaction(&payload, 42, None, std::slice::from_ref(&record))
                .unwrap();
        assert_eq!(converted.source, encoded);
        assert_eq!(converted.accepted_bytes, encoded.len());
        assert_eq!(converted.reserved_requests, 4);
        assert!(convert_character_compaction(&payload, 42, None, &[record + "changed"]).is_err());
    }
}
