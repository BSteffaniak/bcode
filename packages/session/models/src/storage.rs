//! Portable physical session-storage measurements.

use serde::{Deserialize, Serialize};

/// Filesystem bytes for one storage category, not database row or logical content bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStorageBytes {
    /// Number of regular files measured. Hard links are counted per directory entry.
    pub files: u64,
    /// Sum of file lengths, including sparse extents.
    pub file_bytes: u64,
    /// Allocated filesystem bytes, when the platform exposes this measurement.
    pub allocated_bytes: Option<u64>,
}

/// A bounded, non-transactional observation of one session's storage.
///
/// Versioned by the enclosing application protocol. This is diagnostic information, not
/// canonical validation or an estimate of achievable compression savings. Files may change during
/// measurement. Search provider indexes and the global catalog are not session-local files and are
/// excluded; database bytes include both canonical events and derived projections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStorageUsage {
    /// Canonical database and its database-engine sidecars.
    pub database: SessionStorageBytes,
    /// Session-owned terminal recordings, tool output, and other artifact files.
    pub artifacts: SessionStorageBytes,
    /// Other session-local files, including retained migration backups and ownership metadata.
    pub other: SessionStorageBytes,
    /// Directory entries visited, including directories and skipped entries.
    pub visited_entries: u32,
    /// Entries that were not safe or possible to inspect. Their contents are excluded.
    pub skipped_entries: u32,
    /// Whether traversal exhausted the requested entry budget before finishing.
    pub budget_exhausted: bool,
}
