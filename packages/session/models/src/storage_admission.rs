//! Explicit storage-admission inspection and recovery contracts.
use serde::{Deserialize, Serialize};

/// Bounded observation; unknown representations are never retired.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageAdmissionReport {
    /// Number of dirty or empty interrupted participants with no live owner.
    pub abandoned: u32,
    /// Known clean participants.
    pub clean: u32,
    /// Gate or participant ownership could not be acquired.
    pub busy: bool,
    /// Unknown, malformed, unsafe or over-budget evidence blocks recovery.
    pub invalid: bool,
    /// Participants retired after access age was durably reset.
    pub retired: u32,
}
