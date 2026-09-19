//! Request-only source context for isolated structured generation.
use serde::{Deserialize, Serialize};

/// Versioned request to prepare an isolated session with pinned request-only context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareContextGeneration {
    /// Contract version, currently 1.
    pub version: u32,
    /// Session whose normal model context is captured.
    pub source_session_id: crate::SessionId,
    /// Display name of the isolated generation session.
    pub name: String,
}

/// Provenance and destination for one isolated context-aware generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedContextGeneration {
    /// Isolated destination; source messages are not copied into its event history.
    pub session_id: crate::SessionId,
    /// Source identity, generation, cutoff and working directory.
    pub source: crate::SessionDerivationSourceSnapshot,
    /// Opaque ephemeral capability, invalid after daemon replacement.
    pub context_id: String,
}
