//! Portable contracts for explicitly approved reuse of external coding-agent credentials.
//!
//! These types contain source identities and consent metadata only, never credential values.
use serde::{Deserialize, Serialize};

/// Portable application boundary for explicit external-auth consent operations.
/// Implementations authorize requests before mutation and never return credential material.
pub trait ExternalAuthApplication {
    /// Discover non-secret source metadata from currently enabled declarations.
    fn discover_external_auth(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ExternalAuthCandidate>, ExternalAuthError>> + Send;
    /// Persist a user-approved binding; absent scope defaults to all local applications.
    fn approve_external_auth(
        &self,
        candidate: ExternalAuthCandidate,
        scope: Option<ExternalAuthScope>,
    ) -> impl std::future::Future<Output = Result<ExternalAuthGrant, ExternalAuthError>> + Send;
    /// List non-secret durable approvals, including stale approvals available for revocation.
    fn list_external_auth(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ExternalAuthGrant>, ExternalAuthError>> + Send;
    /// Revoke exactly the supplied approval without reading its source.
    fn revoke_external_auth(
        &self,
        grant: ExternalAuthGrant,
    ) -> impl std::future::Future<Output = Result<bool, ExternalAuthError>> + Send;
}

/// Version of the durable external-auth consent document.
pub const EXTERNAL_AUTH_SCHEMA_VERSION: u16 = 1;

/// Supported external application credential source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAuthSourceKind {
    /// Plugin-declared application identity; the host does not classify source paths or names.
    Declared { application: String },
    /// Claude Code local credential store.
    ClaudeCode,
    /// Codex CLI local credential store.
    Codex,
    /// Pi coding-agent local credential store.
    Pi,
    /// `OpenCode` local credential store.
    OpenCode,
}

impl ExternalAuthSourceKind {
    /// Stable source identifier, suitable for user-facing diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Declared { application } => application,
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::OpenCode => "opencode",
        }
    }
}

/// Consent applies to this application boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalAuthScope {
    /// Default: any local Bcode consumer may use the approved binding.
    AllLocalApplications,
    /// Restrict consent to the named local Bcode application.
    Application { application_id: String },
}

/// Current provider declaration target. Revalidated against enabled declarations at use time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAuthTarget {
    /// Enabled plugin owner identity.
    pub plugin_id: String,
    /// Declared provider identity within that plugin.
    pub provider_id: String,
    /// Declared method identity.
    pub method_id: String,
    /// Declared credential field identity.
    pub credential_id: String,
}

/// Sanitized discovery result; deliberately excludes secret-bearing data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAuthCandidate {
    /// External application type.
    pub source: ExternalAuthSourceKind,
    /// SHA-256 of the exact current provider declaration and JSON source.
    pub binding_id: String,
    /// Current declaration target.
    pub target: ExternalAuthTarget,
    /// Display-only description, never a persistent source identity.
    pub label: String,
    /// Whether the source was safely detected without acquiring its secret.
    pub available: bool,
}

/// Approved source-to-provider binding persisted by Bcode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAuthGrant {
    /// Source kind (identity is not a discovery index or display label).
    pub source: ExternalAuthSourceKind,
    /// Exact declaration/source digest approved by the user.
    pub binding_id: String,
    /// Declaration target revalidated before any source read.
    pub target: ExternalAuthTarget,
    /// Consent scope.
    pub scope: ExternalAuthScope,
    /// Unix timestamp when approved.
    pub approved_at_unix: u64,
}

/// Durable, versioned non-secret consent state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAuthState {
    /// Must equal [`EXTERNAL_AUTH_SCHEMA_VERSION`].
    pub schema_version: u16,
    /// Explicit approvals only.
    pub grants: Vec<ExternalAuthGrant>,
}

impl Default for ExternalAuthState {
    fn default() -> Self {
        Self {
            schema_version: EXTERNAL_AUTH_SCHEMA_VERSION,
            grants: Vec::new(),
        }
    }
}

/// Portable operation failure; secret-free and safe to present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAuthError {
    /// Durable state uses an unsupported or malformed version.
    UnsupportedState,
    /// No matching explicit approval exists.
    NotApproved,
    /// Source is absent or cannot be read safely.
    SourceUnavailable,
    /// Enabled plugin declaration is absent or changed.
    DeclarationChanged,
    /// Scope does not authorize this application.
    ScopeDenied,
    /// Consent state could not be safely persisted or read.
    StateUnavailable,
    /// Requested scope or identity is malformed.
    InvalidRequest,
}
