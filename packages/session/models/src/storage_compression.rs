//! Explicit bounded storage compression contracts; compatibility follows the enclosing protocol.
use crate::SessionId;
use serde::{Deserialize, Serialize};

/// Requested storage encoding strength; never a request to downgrade existing storage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageCompressionTier {
    /// Fast compression.
    #[default]
    Light,
    /// Stronger compression.
    Deep,
}

/// Continuation of a finite per-session sweep, not a durable operation handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageCompressionCursor {
    /// Finalized artifact page.
    Artifacts {
        /// Exclusive reference identity.
        after: Option<(String, String)>,
        /// Captured canonical tail.
        through: Option<u64>,
    },
    /// Canonical history page.
    History {
        /// Inclusive sequence.
        start: u64,
        /// Captured canonical tail.
        through: u64,
    },
}

/// Explicit compression policy for one bounded request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCompressionRequest {
    /// Owning session.
    pub session_id: SessionId,
    /// Fixed invocation time; future times are rejected by the server.
    pub as_of_ms: u64,
    /// Inactivity requirement; absent for explicit ID selection without an age filter.
    pub minimum_age_ms: Option<u64>,
    /// Compression strength.
    pub tier: StorageCompressionTier,
    /// Eligibility observation only; no writes or tracking initialization.
    pub dry_run: bool,
    /// Absent for a new sweep.
    pub cursor: Option<StorageCompressionCursor>,
}

/// Structured per-page result; skips do not imply successful compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageCompressionDisposition {
    /// Eligible at observation time; execution must recheck.
    Eligible,
    /// Page processed, including already compressed or insufficient-savings candidates.
    Processed,
    /// Session was used too recently or its clock is ahead of the cutoff.
    Recent,
    /// No trustworthy access age exists.
    UnknownAge,
    /// Ownership, compatibility, integrity or availability prevented this page.
    Unavailable,
}

/// Secret-safe failure categories for explicit compression. No paths or raw engine errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageCompressionFailure {
    /// Another operation holds the shared admission gate.
    AdmissionBusy,
    /// Another live or uncleanly stopped daemon has not acknowledged maintenance.
    UnacknowledgedDaemon,
    /// Admission evidence is damaged, dirty, incomplete, or unavailable.
    AdmissionUnavailable,
    /// Current daemon registration or tracking health is unavailable.
    TrackingUnavailable,
    /// Canonical storage is unavailable or ambiguous.
    StorageUnavailable,
    /// Candidate discovery failed ownership, compatibility, or projection validation.
    CandidateInspectionFailed,
    /// An artifact rewrite failed validation, I/O, or its work allowance.
    ArtifactFailed,
    /// A history rewrite failed validation, I/O, or its work allowance.
    HistoryFailed,
}

impl StorageCompressionFailure {
    /// Stable, secret-safe explanation and next action.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::AdmissionBusy => {
                "storage admission is busy; wait for active reads or maintenance to finish and retry"
            }
            Self::UnacknowledgedDaemon => {
                "another live or unclean daemon registration blocks maintenance; cleanly stop other daemons sharing this state location; do not delete registry files"
            }
            Self::AdmissionUnavailable => {
                "storage admission evidence is unavailable, dirty, or invalid; inspect storage coordination health; do not delete registry files"
            }
            Self::TrackingUnavailable => {
                "this daemon's storage tracking or registration is unhealthy; inspect daemon diagnostics before retrying"
            }
            Self::StorageUnavailable => {
                "canonical storage is unavailable or its location is ambiguous; inspect session ownership and location"
            }
            Self::CandidateInspectionFailed => {
                "candidate inspection failed; inspect session ownership, compatibility, and derived-state health before retrying"
            }
            Self::ArtifactFailed => {
                "artifact compression failed validation, I/O, or its work allowance; this session sweep stopped; inspect artifact storage before retrying"
            }
            Self::HistoryFailed => {
                "history compression failed validation, I/O, or its work allowance; this session sweep stopped; inspect session storage before retrying"
            }
        }
    }
}

/// Bounded artifact failure context. Identity strings are rendered escaped by frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageArtifactFailure {
    /// Logical artifact ID, never a resolved path.
    pub artifact_id: String,
    /// Logical reference key.
    pub reference_key: String,
    /// Normalized stage or cause; no raw engine error text.
    pub reason: ArtifactCompressionFailureReason,
}

/// Secret-safe artifact compression failure reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactCompressionFailureReason {
    /// Reference is missing from the finalized projection.
    ReferenceMissing,
    /// Finalization/completeness metadata is not sufficient for compression.
    NotFinalized,
    /// No logical byte length was recorded.
    MissingLength,
    /// Storage URI is missing or cannot be safely resolved.
    InvalidReference,
    /// Content length disagrees with the reference.
    LengthMismatch,
    /// Content checksum disagrees with the reference.
    ChecksumMismatch,
    /// Reading the current reference failed compatibility or projection validation.
    ReferenceInspection,
    /// Session maintenance ownership could not be acquired.
    OwnershipUnavailable,
    /// Durable read admission could not be acquired.
    AdmissionUnavailable,
    /// Referenced storage does not exist.
    NotFound,
    /// Filesystem access was denied.
    PermissionDenied,
    /// A required resource is busy.
    Busy,
    /// Stored content or representation is invalid.
    InvalidData,
    /// Work exceeded the configured request allowance.
    Timeout,
    /// Work was cancelled.
    Cancelled,
    /// The platform or representation is unsupported.
    Unsupported,
    /// Other I/O or publication failure; prior committed publication may be retained.
    Io,
}

/// Bounded result, versioned by the enclosing application protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCompressionResult {
    /// Session identity.
    pub session_id: SessionId,
    /// Outcome of this page.
    pub disposition: StorageCompressionDisposition,
    /// Physical artifact representation bytes saved, not allocated disk savings.
    pub artifact_bytes_saved: u64,
    /// Stored history payload bytes saved, not database-file shrinkage.
    pub history_payload_bytes_saved: u64,
    /// Candidate operations that failed; successful earlier work is retained.
    pub failures: u32,
    /// True when the current page changed a representation.
    pub changed: bool,
    /// Structured reason for a failed page; omitted for successful or age-filtered pages.
    pub failure: Option<StorageCompressionFailure>,
    /// Specific artifact failure context when available.
    pub artifact_failure: Option<StorageArtifactFailure>,
    /// Next page, if any. No durable resume is promised.
    pub next: Option<StorageCompressionCursor>,
}

/// Parse explicit positive durations such as `12h`, `7d`, or `2w`.
///
/// # Errors
/// Rejects missing units, zero, negative, fractional, overflowing or unsupported durations.
pub fn parse_storage_compression_age(value: &str) -> Result<u64, String> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix('h') {
        (digits, 3_600_000_u64)
    } else if let Some(digits) = value.strip_suffix('d') {
        (digits, 86_400_000)
    } else if let Some(digits) = value.strip_suffix('w') {
        (digits, 604_800_000)
    } else {
        return Err("use a positive duration with h, d, or w (for example 14d)".into());
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("duration must contain positive whole units".into());
    }
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| *n > 0)
        .ok_or_else(|| "duration is zero or overflows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_duration_units_and_overflow() {
        assert_eq!(parse_storage_compression_age("12h"), Ok(43_200_000));
        assert_eq!(
            parse_storage_compression_age("7d"),
            parse_storage_compression_age("1w")
        );
        for value in ["0d", "-1h", "14", "1.5d", "d", "18446744073709551615w"] {
            assert!(parse_storage_compression_age(value).is_err());
        }
    }
}
