//! Operation-time receipt for bytes accepted by a successful filesystem write.

use serde::Serialize;
use sha2::{Digest as _, Sha256};

#[derive(Serialize)]
struct WriteReceipt {
    schema: &'static str,
    version: u32,
    phase: &'static str,
    byte_len: usize,
    sha256: String,
    later_freshness: &'static str,
    downstream_consumption: &'static str,
}

/// Describe supplied bytes only after the write operation succeeds, without rereading the path.
pub fn model_output(summary: &str, contents: &str) -> String {
    let receipt = WriteReceipt {
        schema: "bcode.filesystem.write_receipt",
        version: 1,
        phase: "written_at_operation",
        byte_len: contents.len(),
        sha256: hex::encode(Sha256::digest(contents.as_bytes())),
        later_freshness: "not_assessed",
        downstream_consumption: "not_assessed",
    };
    // Do not duplicate supplied content in model output. The existing FileChange artifact owns it.
    format!(
        "{summary}\n{}",
        serde_json::to_string(&receipt).expect("write receipt serialization")
    )
}

#[cfg(test)]
mod tests;
