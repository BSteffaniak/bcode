#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral managed harness protocol. Identity is not execution authority.
//! Host admission and durable ownership fencing must precede every mutation.
//! Unknown versions and variants fail closed; an uncertain receipt never authorizes replay.

mod lifecycle;
mod service;
pub use lifecycle::*;
pub use service::*;

use serde::{Deserialize, Serialize};

/// Independently versioned plugin service boundary.
pub const HARNESS_INTERFACE_ID: &str = "bcode.harness/v1";
/// Current envelope schema; other versions are rejected, not coerced.
pub const HARNESS_SCHEMA_VERSION: u16 = 1;
/// Maximum encoded request or response, checked before decoding.
pub const MAX_HARNESS_WIRE_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes in an identity/cursor.
pub const MAX_HARNESS_ID_BYTES: usize = 256;
/// Maximum UTF-8 bytes in semantic text.
pub const MAX_HARNESS_TEXT_BYTES: usize = 64 * 1024;
/// Maximum observations in one page.
pub const MAX_HARNESS_OBSERVATIONS: usize = 128;
/// Maximum choices, capability constraints, or native correlations.
pub const MAX_HARNESS_ITEMS: usize = 32;
/// Maximum serialized vendor detail bytes.
pub const MAX_HARNESS_VENDOR_BYTES: usize = 16 * 1024;

/// Secret-safe protocol failure. Never contains raw native payloads or input values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessContractError {
    UnsupportedVersion,
    InvalidIdentity,
    BoundsExceeded,
    InvalidShape,
    StaleGeneration,
    WrongOwner,
    OperationMismatch,
}
impl std::fmt::Display for HarnessContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "harness contract violation: {self:?}")
    }
}
impl std::error::Error for HarnessContractError {}

pub(crate) fn id(value: &str) -> Result<(), HarnessContractError> {
    if value.is_empty()
        || value.len() > MAX_HARNESS_ID_BYTES
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        return Err(HarnessContractError::InvalidIdentity);
    }
    Ok(())
}
pub(crate) const fn text(value: &str) -> Result<(), HarnessContractError> {
    if value.len() > MAX_HARNESS_TEXT_BYTES {
        return Err(HarnessContractError::BoundsExceeded);
    }
    Ok(())
}
pub(crate) fn wire<T: Serialize>(value: &T, maximum: usize) -> Result<(), HarnessContractError> {
    // Count without allocating an unbounded encoded buffer.
    struct Counter {
        bytes: usize,
        maximum: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.saturating_add(bytes.len());
            if self.bytes > self.maximum {
                return Err(std::io::Error::other("bounded serialization exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { bytes: 0, maximum }, value)
        .map_err(|_| HarnessContractError::BoundsExceeded)
}
