//! Portable, secret-free startup authentication choices.

use serde::{Deserialize, Serialize};

/// One provider-declared source, never credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartupCredentialSource {
    /// Request-local use is authorized by the field's invocation declaration.
    Environment { name: String },
    /// Explicit import is required before this source can be used.
    Import { source_index: usize, label: String },
}

/// Local availability does not prove remote authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupCredentialAvailability {
    /// Local validation succeeded.
    Available,
    /// Source was absent.
    Missing,
    /// Source could not be safely read or validated.
    Unavailable,
}

/// Sanitized choice discovered from an enabled provider contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupAuthCandidate {
    /// Registered provider identity.
    pub provider_id: String,
    /// Provider-contributed display name.
    pub provider_name: String,
    /// Provider-local authentication method.
    pub method_id: String,
    /// Method-contributed display name.
    pub method_name: String,
    /// Canonical credential field identity.
    pub credential_id: String,
    /// Non-secret source reference.
    pub source: StartupCredentialSource,
    /// Sanitized local availability.
    pub availability: StartupCredentialAvailability,
    /// False when other mandatory fields require enrollment as well.
    pub completes_method: bool,
}
