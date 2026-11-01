//! Host-retained bounded payload assembly. Plugin references never name private paths.
#[cfg(test)]
mod tests;
use super::Entry;
use bcode_plugin_sdk::diagnostics::payload::{
    MAX_CHUNK_BYTES, MAX_PAYLOAD_BYTES, MAX_TRANSFERS, PAYLOAD_VERSION, PayloadControl,
    PayloadReply,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const GLOBAL_BYTES: usize = 16 * MAX_PAYLOAD_BYTES;
const TTL: Duration = Duration::from_secs(30);
static RESERVED: AtomicUsize = AtomicUsize::new(0);

struct Transfer {
    expected: usize,
    digest: String,
    media_type: String,
    bytes: Vec<u8>,
    started: Instant,
}
impl Drop for Transfer {
    fn drop(&mut self) {
        RESERVED.fetch_sub(self.expected, Ordering::AcqRel);
    }
}

#[derive(Default)]
pub(super) struct Transfers {
    negotiated: bool,
    next_id: u64,
    pending: BTreeMap<u64, Transfer>,
}

impl Transfers {
    pub(super) fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

fn loss(entry: &Entry, reason: &'static str, count: u64) {
    let _ = entry.capture.report_external_loss(count);
    entry.capture.emit(
        "plugin",
        "diagnostic_bridge_gap",
        serde_json::json!({"reason": reason, "unavailable_callbacks": count, "complete": false}),
        false,
    );
}

fn reject(entry: &Entry, reason: &'static str) -> PayloadReply {
    loss(entry, reason, 1);
    PayloadReply::Unavailable
}

pub(super) fn control(entry: &Entry, operation: PayloadControl) -> PayloadReply {
    let Ok(mut state) = entry.payloads.try_lock() else {
        return reject(entry, "payload_registry_contention");
    };
    let before = state.pending.len();
    state
        .pending
        .retain(|_, transfer| transfer.started.elapsed() < TTL);
    if before != state.pending.len() {
        let expired = before - state.pending.len();
        loss(entry, "payload_transfer_expired", expired as u64);
    }
    // Authorization precedes allocation, decoding acceptance and retention. Recheck on commit.
    if !entry.capture.deep_capture_active() {
        if !state.pending.is_empty() {
            let _ = entry
                .capture
                .report_external_loss(state.pending.len() as u64);
        }
        state.pending.clear();
        return PayloadReply::Unavailable;
    }
    if let PayloadControl::Negotiate { version } = operation {
        if version != PAYLOAD_VERSION {
            state.negotiated = false;
            if !state.pending.is_empty() {
                loss(
                    entry,
                    "payload_negotiation_reset",
                    state.pending.len() as u64,
                );
            }
            state.pending.clear();
            return reject(entry, "unsupported_payload_version");
        }
        state.negotiated = true;
        return PayloadReply::Supported {
            version: PAYLOAD_VERSION,
            max_bytes: MAX_PAYLOAD_BYTES,
            chunk_bytes: MAX_CHUNK_BYTES,
        };
    }
    if !state.negotiated {
        return reject(entry, "payload_not_negotiated");
    }
    match operation {
        PayloadControl::Negotiate { .. } => unreachable!(),
        PayloadControl::Begin {
            byte_len,
            sha256,
            media_type,
        } => begin(entry, &mut state, byte_len, sha256, media_type),
        PayloadControl::Chunk {
            transfer,
            offset,
            bytes,
        } => {
            let valid = state.pending.get(&transfer).is_some_and(|value| {
                !bytes.is_empty()
                    && bytes.len() <= MAX_CHUNK_BYTES
                    && offset == value.bytes.len()
                    && offset
                        .checked_add(bytes.len())
                        .is_some_and(|end| end <= value.expected)
            });
            if !valid {
                state.pending.remove(&transfer);
                return reject(entry, "payload_chunk_order_or_quota");
            }
            if let Some(value) = state.pending.get_mut(&transfer) {
                value.bytes.extend_from_slice(&bytes);
            }
            PayloadReply::Progress
        }
        PayloadControl::Abort { transfer } => {
            if state.pending.remove(&transfer).is_some() {
                loss(entry, "payload_transfer_aborted", 1);
            }
            PayloadReply::Progress
        }
        PayloadControl::Finish { transfer } => {
            let Some(value) = state.pending.remove(&transfer) else {
                return reject(entry, "payload_unknown_transfer");
            };
            drop(state);
            if value.bytes.len() != value.expected
                || hex::encode(Sha256::digest(&value.bytes)) != value.digest
            {
                return reject(entry, "payload_length_or_digest_mismatch");
            }
            // The engine assigns the payload/session identity and enforces current observation
            // retention. Never trust plugin origin, sensitivity flags or payload reference JSON.
            match entry
                .capture
                .capture_payload(&value.media_type, &value.bytes)
            {
                Ok(Some(reference)) => PayloadReply::Retained { reference },
                _ => reject(entry, "payload_retention_unavailable"),
            }
        }
    }
}

fn begin(
    entry: &Entry,
    state: &mut Transfers,
    byte_len: usize,
    sha256: String,
    media_type: String,
) -> PayloadReply {
    if byte_len > MAX_PAYLOAD_BYTES
        || state.pending.len() >= MAX_TRANSFERS
        || sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || media_type.is_empty()
        || media_type.len() > 128
        || !media_type.bytes().all(|b| b.is_ascii_graphic())
    {
        return reject(entry, "invalid_payload_begin_or_quota");
    }
    let Some(id) = state.next_id.checked_add(1) else {
        return reject(entry, "payload_id_exhausted");
    };
    if RESERVED
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(byte_len)
                .filter(|total| *total <= GLOBAL_BYTES)
        })
        .is_err()
    {
        return reject(entry, "payload_global_quota");
    }
    state.next_id = id;
    state.pending.insert(
        id,
        Transfer {
            expected: byte_len,
            digest: sha256,
            media_type,
            bytes: Vec::with_capacity(byte_len),
            started: Instant::now(),
        },
    );
    PayloadReply::Accepted { transfer: id }
}
