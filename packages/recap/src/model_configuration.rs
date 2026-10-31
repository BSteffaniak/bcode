//! Portable, secret-free standalone inference settings retained for generation resume.
use serde::{Deserialize, Serialize};

/// Model configuration references, never configuration contents or credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfiguration {
    /// Explicit configuration file; empty means normal Bcode configuration.
    pub config_path: std::path::PathBuf,
    /// Configured model profile reference.
    pub profile: Option<String>,
    /// Configuration context reference.
    pub context: Option<String>,
    /// Centrally resolved model name.
    pub model: Option<String>,
    /// Centrally resolved provider name.
    pub provider: Option<String>,
    /// Validated credential-free endpoint override.
    pub endpoint: Option<String>,
    /// Authentication profile reference, never its credentials.
    pub auth_profile: Option<String>,
    /// Maximum output tokens per request.
    pub max_output_tokens: u32,
    /// Deadline for one request.
    pub timeout: std::time::Duration,
}

/// Versioned settings bound to a generation's original executor identity.
/// Explicit model overrides affect unfinished work, not already completed calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeModelConfiguration {
    /// Compatibility boundary; only version one is supported.
    pub version: u32,
    /// Original executor identity recorded by the generation header.
    pub identity: String,
    /// Effective pinned selection and execution settings used for unfinished work.
    pub options: ModelConfiguration,
    /// Hash of the fully resolved provider/model/profile selection.
    pub selection_fingerprint: String,
    /// Fingerprint of the effective referenced configuration, including selection.
    /// Only hashes are retained; configuration contents and credentials are excluded.
    pub config_fingerprint: Option<String>,
}
